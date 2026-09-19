//! Native provider execution inside Weld's shared stream-affine decode pool.
use super::{
    Shared,
    frame::{Frame, FrameBudget, FrameCredit},
};
use crate::native::{self, Progress};
use crate::presentation::supported_extent;
use anyhow::{Context, Result, ensure};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, atomic::Ordering},
    task::Poll,
    thread,
    time::{Duration, Instant},
};
use weld_hoist_encoded::{
    DecodeBackend, DecodeCompletion, DecodeRequest, DecodedFrame, SubmitError,
};
use weld_media::{
    DecoderConfig, MediaFrameId, MediaStreamId, StreamGeneration, VideoCodec, WorkerSubmitError,
    decode::{DecodeJob, DecodePool, DecodePoolLimits, DecodeProcessor},
};

type Generation = (MediaStreamId, StreamGeneration);

pub(super) struct Backend {
    pool: DecodePool<Processor>,
    credits: Arc<FrameBudget>,
    shared: Arc<Shared>,
    codec: VideoCodec,
    streams: HashMap<MediaStreamId, HashSet<StreamGeneration>>,
}
impl Backend {
    pub fn new(
        target: native::Target,
        shared: Arc<Shared>,
        credits: Arc<FrameBudget>,
        codec: VideoCodec,
        low_latency: bool,
    ) -> Result<Self> {
        let owner = thread::current();
        let context = shared.clone();
        Ok(Self {
            pool: DecodePool::new(
                DecodePoolLimits::try_new(2, 4, 16, 2)?,
                move || {
                    Ok(Processor {
                        target: target.clone(),
                        shared: context.clone(),
                        decoders: HashMap::new(),
                        pending: VecDeque::new(),
                        low_latency,
                    })
                },
                move || owner.unpark(),
            ),
            shared,
            credits,
            codec,
            streams: HashMap::new(),
        })
    }
    fn reject(&self, error: impl Into<String>) -> SubmitError<DecodeRequest> {
        let error = error.into();
        self.shared.fail(&error);
        SubmitError::Rejected(anyhow::anyhow!(error))
    }
}
impl DecodeBackend for Backend {
    type Output = Frame;
    fn try_submit(&mut self, request: DecodeRequest) -> Result<(), SubmitError<DecodeRequest>> {
        if request.access_unit.codec != self.codec {
            return Err(self.reject("stream codec differs from negotiated codec"));
        }
        if !supported_extent(request.visible_width, request.visible_height) {
            return Err(self
                .reject("phone tracer supports at most 2048 per dimension and 1920x1080 pixels"));
        }
        let stream = request.access_unit.frame.stream;
        let generation = request.access_unit.frame.generation;
        if self.streams.len() >= 8 && !self.streams.contains_key(&stream) {
            return Err(self.reject("receiver active stream budget exceeded (8)"));
        }
        let Some(credit) = self.credits.reserve(stream) else {
            return Err(SubmitError::Busy(request));
        };
        self.pool
            .try_decode(Job { request, credit })
            .map_err(|error| match error {
                WorkerSubmitError::Busy(job) => {
                    let Job { request, credit } = *job;
                    credit.cancel();
                    self.shared.message(
                        "Waiting for decoder job/generation capacity (image credit available)",
                    );
                    SubmitError::Busy(request)
                }
                WorkerSubmitError::Stopped(job) => {
                    let Job { request, credit } = *job;
                    credit.cancel();
                    SubmitError::Stopped(request)
                }
                WorkerSubmitError::Rejected(error) => {
                    self.reject(format!("decoder admission: {error:#}"))
                }
            })?;
        self.streams.entry(stream).or_default().insert(generation);
        Ok(())
    }
    fn drain(&mut self) -> (Vec<DecodeCompletion<Frame>>, Option<anyhow::Error>) {
        let (completions, failure) = self.pool.drain();
        if let Some(error) = &failure {
            self.shared.fail(format!("decoder worker: {error:#}"));
        }
        let completions = completions
            .into_iter()
            .map(|completion| {
                if let Err(error) = &completion.result {
                    self.shared.fail(format!("decoder: {error:#}"));
                }
                DecodeCompletion {
                    token: completion.token,
                    result: completion.result,
                    timing: completion.timing,
                }
            })
            .collect();
        (completions, failure)
    }
    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()> {
        self.pool.retire(stream, generation)?;
        if let Some(generations) = self.streams.get_mut(&stream) {
            generations.remove(&generation);
            if generations.is_empty() {
                self.streams.remove(&stream);
            }
        }
        Ok(())
    }
}

struct Job {
    request: DecodeRequest,
    credit: FrameCredit,
}
impl DecodeJob for Job {
    fn token(&self) -> u64 {
        self.request.token
    }
    fn frame(&self) -> MediaFrameId {
        self.request.access_unit.frame
    }
}
struct Stream {
    decoder: native::Decoder,
    extent: [u32; 2],
    codec: VideoCodec,
    accepted: VecDeque<(u64, usize)>,
    returned: VecDeque<u64>,
}
impl Stream {
    fn try_send(&mut self, payload: &[u8], sequence: u64) -> Result<bool> {
        let accepted = self.decoder.try_send(payload, sequence)?;
        if accepted {
            if self.accepted.len() == 16 {
                self.accepted.pop_front();
            }
            self.accepted.push_back((sequence, payload.len()));
        }
        Ok(accepted)
    }

    fn receive(&mut self, cancelled: impl Fn() -> bool) -> Result<Progress> {
        let progress = self.decoder.receive(cancelled)?;
        if let Progress::Image(image) = &progress {
            if self.returned.len() == 16 {
                self.returned.pop_front();
            }
            self.returned.push_back(image.timestamp_micros());
        }
        Ok(progress)
    }
}
struct Submitted<I = native::Image> {
    // Return native storage before releasing this job's admission credit.
    image: Option<I>,
    job: Job,
    accepted: bool,
    deadline: Option<Instant>,
}

/// Restore the worker's FIFO completion contract without adding image credits
/// or a separate queue. Native output can only occupy an accepted job's slot.
fn route_output<I>(
    head: &Submitted<I>,
    pending: &mut VecDeque<Result<Submitted<I>>>,
    frame: MediaFrameId,
    image: I,
) -> Result<Option<I>> {
    let expected = head.job.frame();
    ensure!(
        (frame.stream, frame.generation) == (expected.stream, expected.generation),
        "decoded output belongs to another decoder generation"
    );
    if frame == expected {
        ensure!(
            head.accepted,
            "decoded output belongs to an unaccepted head job"
        );
        return Ok(Some(image));
    }
    let next = pending
        .iter_mut()
        .filter_map(|entry| entry.as_mut().ok())
        .find(|entry| entry.job.frame() == frame)
        .context("decoded output does not match an outstanding job")?;
    ensure!(next.accepted, "decoded output belongs to an unaccepted job");
    ensure!(
        next.image.is_none(),
        "duplicate decoded output for a pending job"
    );
    next.image = Some(image);
    tracing::debug!(target: "weld_vr_diag", stream = frame.stream.raw(),
        generation = frame.generation.raw(), expected_sequence = expected.sequence,
        received_sequence = frame.sequence, "buffered out-of-order decoder output");
    Ok(None)
}

fn completion_deadline(deadline: &mut Option<Instant>, now: Instant) -> Instant {
    *deadline.get_or_insert(now + Duration::from_secs(3))
}
struct Processor {
    target: native::Target,
    shared: Arc<Shared>,
    decoders: HashMap<Generation, Stream>,
    pending: VecDeque<Result<Submitted>>,
    low_latency: bool,
}
impl Processor {
    fn prepare_stream(&mut self, job: &Job) -> Result<&mut Stream> {
        let request = &job.request;
        let frame = request.access_unit.frame;
        let key = (frame.stream, frame.generation);
        let extent = [request.visible_width, request.visible_height];
        let codec = request.access_unit.codec;
        if let std::collections::hash_map::Entry::Vacant(entry) = self.decoders.entry(key) {
            let config = DecoderConfig::new(codec, extent[0], extent[1], Vec::new())?;
            entry.insert(Stream {
                decoder: native::Decoder::new_with_low_latency(
                    &config,
                    self.target.clone(),
                    self.low_latency,
                    &request.access_unit.payload,
                )?,
                extent,
                codec,
                accepted: VecDeque::with_capacity(16),
                returned: VecDeque::with_capacity(16),
            });
        }
        let stream = self
            .decoders
            .get_mut(&key)
            .context("decoder generation missing")?;
        ensure!(
            stream.extent == extent && stream.codec == codec,
            "codec/extent changed within one generation"
        );
        Ok(stream)
    }

    fn submit_native(&mut self, job: Job) -> Result<Submitted> {
        let frame = job.request.access_unit.frame;
        let key = (frame.stream, frame.generation);
        // Context creation can be slow. Do it only when this job reaches the
        // head, never while an older job's completion deadline is running.
        // Likewise, a later packet may not pass an unaccepted earlier packet.
        let blocked = self.pending.iter().any(|pending| {
            pending.as_ref().is_ok_and(|pending| {
                let earlier = pending.job.request.access_unit.frame;
                (earlier.stream, earlier.generation) == key && !pending.accepted
            })
        });
        let accepted = if !blocked && self.decoders.contains_key(&key) {
            self.prepare_stream(&job)?
                .try_send(&job.request.access_unit.payload, frame.sequence)?
        } else {
            false
        };
        Ok(Submitted {
            image: None,
            job,
            accepted,
            deadline: None,
        })
    }
}
impl DecodeProcessor for Processor {
    type Request = Job;
    type Output = DecodedFrame<Frame>;
    fn submit(&mut self, request: Job) {
        let result = self.submit_native(request);
        self.pending.push_back(result);
    }
    fn poll(&mut self) -> Result<Poll<Vec<Self::Output>>> {
        let mut submitted = self
            .pending
            .pop_front()
            .context("missing submitted decode")??;
        let shared = self.shared.clone();
        ensure!(
            !shared.session.cancelled.load(Ordering::Acquire),
            "decode cancelled"
        );
        // Preserve the existing deadline start after potentially slow native
        // context creation. Subsequent polls and reordered outputs never reset it.
        self.prepare_stream(&submitted.job)?;
        let deadline = completion_deadline(&mut submitted.deadline, Instant::now());
        let frame = submitted.job.frame();
        let image = loop {
            let successor_cached = self.pending.iter().any(|entry| {
                entry.as_ref().is_ok_and(|entry| {
                    let next = entry.job.frame();
                    (next.stream, next.generation) == (frame.stream, frame.generation)
                        && entry.image.is_some()
                })
            });
            ensure!(
                Instant::now() < deadline,
                "decoder did not produce {frame:?} within 3s; successor_cached={successor_cached}"
            );
            ensure!(
                !shared.session.cancelled.load(Ordering::Acquire),
                "decode cancelled"
            );
            if let Some(image) = submitted.image.take() {
                break image;
            }
            let decoder = self.prepare_stream(&submitted.job)?;
            if !submitted.accepted {
                submitted.accepted =
                    decoder.try_send(&submitted.job.request.access_unit.payload, frame.sequence)?;
            }
            match decoder.receive(|| shared.session.cancelled.load(Ordering::Acquire))? {
                Progress::Pending => {
                    self.pending.push_front(Ok(submitted));
                    return Ok(Poll::Pending);
                }
                Progress::End => anyhow::bail!("unexpected decoder end during live stream"),
                Progress::Image(image) => {
                    let returned = MediaFrameId {
                        sequence: image.timestamp_micros(),
                        ..frame
                    };
                    if let Some(image) =
                        route_output(&submitted, &mut self.pending, returned, image).with_context(
                            || format!("matching {returned:?} while awaiting {frame:?}"),
                        )?
                    {
                        break image;
                    }
                    // Drain immediately: the earlier image may already be ready.
                    // Every cached result occupies a unique admitted job, so a
                    // duplicate/unknown result errors instead of extending this loop.
                }
            }
        };
        let output = Frame {
            image,
            visible: [
                submitted.job.request.visible_width,
                submitted.job.request.visible_height,
            ],
            credit: Some(submitted.job.credit),
        };
        output.crop(None)?;
        Ok(Poll::Ready(vec![DecodedFrame {
            frame,
            buffer: output,
        }]))
    }
    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) {
        self.decoders.remove(&(stream, generation));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use weld_media::{EncodedAccessUnit, EncodedFrameKind};

    fn id(stream: u64, generation: u64, sequence: u64) -> MediaFrameId {
        MediaFrameId::new(
            MediaStreamId::new(stream),
            StreamGeneration::new(generation),
            sequence,
        )
    }

    fn submitted<I>(
        budget: &Arc<FrameBudget>,
        frame: MediaFrameId,
        accepted: bool,
    ) -> Submitted<I> {
        Submitted {
            image: None,
            job: Job {
                request: DecodeRequest {
                    token: frame.sequence,
                    access_unit: EncodedAccessUnit {
                        frame,
                        codec: VideoCodec::Av1,
                        kind: EncodedFrameKind::Delta,
                        timestamp_micros: frame.sequence,
                        payload: vec![1],
                    },
                    visible_width: 640,
                    visible_height: 480,
                },
                credit: budget.reserve(frame.stream).expect("image credit"),
            },
            accepted,
            deadline: None,
        }
    }

    #[test]
    fn swapped_outputs_keep_original_jobs_and_complete_in_fifo_order() {
        let budget = FrameBudget::new(thread::current());
        let head = submitted::<u64>(&budget, id(1, 1, 310), true);
        let mut pending = VecDeque::from([Ok(submitted(&budget, id(1, 1, 311), true))]);
        assert!(
            route_output(&head, &mut pending, id(1, 1, 311), 311)
                .unwrap()
                .is_none()
        );
        assert!(head.image.is_none());
        assert_eq!(pending.len(), 1, "reordering never admits another job");
        let first = route_output(&head, &mut pending, id(1, 1, 310), 310).unwrap();
        assert_eq!((head.job.token(), first), (310, Some(310)));
        let mut second = pending.pop_front().unwrap().unwrap();
        // The successor already owns its image: no third input or native
        // receive is necessary to consume the final frame of the stream.
        assert_eq!((second.job.token(), second.image.take()), (311, Some(311)));
        assert!(second.image.take().is_none());
    }

    #[test]
    fn ordinary_output_needs_no_successor_or_extra_storage() {
        let budget = FrameBudget::new(thread::current());
        let head = submitted::<u64>(&budget, id(1, 1, 1), true);
        let mut pending = VecDeque::new();
        assert_eq!(
            route_output(&head, &mut pending, id(1, 1, 1), 1).unwrap(),
            Some(1)
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn output_matching_rejects_unknown_unaccepted_or_cross_generation_images() {
        for (candidate, output, accepted) in [
            (id(2, 1, 311), id(2, 1, 311), true),
            (id(1, 2, 311), id(1, 2, 311), true),
            (id(1, 1, 311), id(1, 1, 312), true),
            (id(1, 1, 311), id(1, 1, 311), false),
        ] {
            let budget = FrameBudget::new(thread::current());
            let head = submitted::<u64>(&budget, id(1, 1, 310), true);
            let mut pending = VecDeque::from([Ok(submitted(&budget, candidate, accepted))]);
            assert!(route_output(&head, &mut pending, output, output.sequence).is_err());
            assert!(pending.front().unwrap().as_ref().unwrap().image.is_none());
        }
        let budget = FrameBudget::new(thread::current());
        let head = submitted::<u64>(&budget, id(1, 1, 310), false);
        assert!(route_output(&head, &mut VecDeque::new(), id(1, 1, 310), 310).is_err());
    }

    #[test]
    fn duplicate_output_never_overwrites_an_owned_image_and_errors_are_not_slots() {
        let budget = FrameBudget::new(thread::current());
        let head = submitted::<u64>(&budget, id(1, 1, 310), true);
        let mut pending = VecDeque::from([
            Err(anyhow::anyhow!("failed submission")),
            Ok(submitted(&budget, id(1, 1, 311), true)),
        ]);
        assert!(
            route_output(&head, &mut pending, id(1, 1, 311), 311)
                .unwrap()
                .is_none()
        );
        assert!(route_output(&head, &mut pending, id(1, 1, 311), 999).is_err());
        assert!(pending.pop_front().unwrap().is_err());
        assert_eq!(pending.pop_front().unwrap().unwrap().image, Some(311));
    }

    struct ImageLeaseProbe {
        budget: Arc<FrameBudget>,
        free_credits_at_drop: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }
    impl Drop for ImageLeaseProbe {
        fn drop(&mut self) {
            let mut available = Vec::new();
            while let Some(credit) = self.budget.reserve(MediaStreamId::new(1)) {
                available.push(credit);
            }
            self.free_credits_at_drop
                .store(available.len(), Ordering::SeqCst);
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn dropping_pending_work_releases_cached_image_before_its_credit() {
        let budget = FrameBudget::new(thread::current());
        let head = submitted::<ImageLeaseProbe>(&budget, id(1, 1, 310), true);
        let mut pending = VecDeque::from([Ok(submitted(&budget, id(1, 1, 311), true))]);
        let mut other_credits = Vec::new();
        while let Some(credit) = budget.reserve(MediaStreamId::new(1)) {
            other_credits.push(credit);
        }
        let free_credits_at_drop = Arc::new(AtomicUsize::new(usize::MAX));
        let dropped = Arc::new(AtomicUsize::new(0));
        let image = ImageLeaseProbe {
            budget: budget.clone(),
            free_credits_at_drop: free_credits_at_drop.clone(),
            dropped: dropped.clone(),
        };
        assert!(
            route_output(&head, &mut pending, id(1, 1, 311), image)
                .unwrap()
                .is_none()
        );
        assert!(budget.reserve(MediaStreamId::new(1)).is_none());
        drop(head);
        drop(pending); // Cached image must drop before its own job's credit.
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(
            free_credits_at_drop.load(Ordering::SeqCst),
            1,
            "only the preceding job's credit is free when the cached image drops"
        );
        let first = budget
            .reserve(MediaStreamId::new(1))
            .expect("head credit released");
        let second = budget
            .reserve(MediaStreamId::new(1))
            .expect("cached job credit released");
        assert!(budget.reserve(MediaStreamId::new(1)).is_none());
        drop((first, second, other_credits));
    }

    #[test]
    fn pending_polls_never_extend_the_original_completion_deadline() {
        let start = Instant::now();
        let mut deadline = None;
        assert_eq!(
            completion_deadline(&mut deadline, start),
            start + Duration::from_secs(3)
        );
        assert_eq!(
            completion_deadline(&mut deadline, start + Duration::from_secs(4)),
            start + Duration::from_secs(3)
        );
    }
    #[test]
    fn tracer_rejects_unbounded_native_allocations() {
        assert!(supported_extent(1280, 833));
        assert!(supported_extent(1920, 1080));
        assert!(!supported_extent(0, 480));
        assert!(!supported_extent(2048, 2048));
        assert!(!supported_extent(8192, 1));
    }
}
