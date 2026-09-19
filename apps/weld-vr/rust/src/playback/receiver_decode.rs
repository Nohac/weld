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
}
struct Submitted {
    job: Job,
    accepted: bool,
    deadline: Option<Instant>,
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
                .decoder
                .try_send(&job.request.access_unit.payload, frame.sequence)?
        } else {
            false
        };
        Ok(Submitted {
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
        let decoder = &mut self.prepare_stream(&submitted.job)?.decoder;
        let deadline = completion_deadline(&mut submitted.deadline, Instant::now());
        ensure!(
            Instant::now() < deadline,
            "decoder did not produce a low-delay frame within 3s"
        );
        let request = &submitted.job.request;
        let frame = request.access_unit.frame;
        if !submitted.accepted {
            submitted.accepted = decoder.try_send(&request.access_unit.payload, frame.sequence)?;
        }
        match decoder.receive(|| shared.session.cancelled.load(Ordering::Acquire))? {
            Progress::Pending => {
                self.pending.push_front(Ok(submitted));
                Ok(Poll::Pending)
            }
            Progress::End => anyhow::bail!("unexpected decoder end during live stream"),
            Progress::Image(image) => {
                ensure!(
                    submitted.accepted && image.timestamp_micros() == frame.sequence,
                    "decoded output belongs to another frame"
                );
                let output = Frame {
                    image,
                    visible: [request.visible_width, request.visible_height],
                    credit: Some(submitted.job.credit),
                };
                output.crop(None)?;
                Ok(Poll::Ready(vec![DecodedFrame {
                    frame,
                    buffer: output,
                }]))
            }
        }
    }
    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) {
        self.decoders.remove(&(stream, generation));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
