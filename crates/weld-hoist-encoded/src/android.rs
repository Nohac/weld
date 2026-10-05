//! Android images produced on Weld's bounded, stream-affine decode workers.
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::Rc,
    task::Poll,
    time::{Duration, Instant},
};

use crate::{
    DecodeBackend, DecodeCompletion, DecodeRequest, DecodedFrame, DecodedFramePublisher,
    SubmitError,
};
use anyhow::{Context, Result, ensure};
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, Extent,
};
use weld_media::{
    MediaFrameId, MediaStreamId, StreamGeneration, VideoCodec, WorkerSubmitError,
    decode::{DecodeJob, DecodePool, DecodePoolLimits, DecodeProcessor},
};
use weld_media_android::{
    AndroidDecoder, AndroidImage, AndroidImageTarget, DecodeProgress, stream_configuration,
};

/// MediaCodec provider using the same bounded worker policy as desktop decoding.
pub struct AndroidDecodeBackend {
    pool: DecodePool<Processor>,
    codec: VideoCodec,
}

impl AndroidDecodeBackend {
    pub fn new(codec: VideoCodec, notify: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            // Native processor capacity is explicit; the portable pool owns
            // admission, stream affinity, wakeups and retirement.
            pool: DecodePool::new(
                DecodePoolLimits::default(),
                || Ok(Processor::default()),
                notify,
            ),
            codec,
        }
    }
}

impl DecodeBackend for AndroidDecodeBackend {
    type Output = AndroidImage;
    fn try_submit(&mut self, request: DecodeRequest) -> Result<(), SubmitError<DecodeRequest>> {
        if request.access_unit.codec != self.codec {
            return Err(SubmitError::Rejected(anyhow::anyhow!(
                "unexpected stream codec"
            )));
        }
        self.pool
            .try_decode(Job(request))
            .map_err(|error| match error {
                WorkerSubmitError::Busy(job) => SubmitError::Busy(job.0),
                WorkerSubmitError::Stopped(job) => SubmitError::Stopped(job.0),
                WorkerSubmitError::Rejected(error) => SubmitError::Rejected(error),
            })
    }
    fn drain(&mut self) -> (Vec<DecodeCompletion<AndroidImage>>, Option<anyhow::Error>) {
        let (items, error) = self.pool.drain();
        (
            items
                .into_iter()
                .map(|item| DecodeCompletion {
                    token: item.token,
                    result: item.result,
                    timing: item.timing,
                })
                .collect(),
            error,
        )
    }
    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()> {
        self.pool.retire(stream, generation)
    }
}

struct Job(DecodeRequest);
impl DecodeJob for Job {
    fn token(&self) -> u64 {
        self.0.token
    }
    fn frame(&self) -> MediaFrameId {
        self.0.access_unit.frame
    }
}
struct Pending {
    job: Job,
    accepted: bool,
    image: Option<AndroidImage>,
    deadline: Option<Instant>,
}

#[derive(Default)]
struct Processor {
    decoders: HashMap<(MediaStreamId, StreamGeneration), AndroidDecoder>,
    pending: VecDeque<Pending>,
}

impl Processor {
    fn prepare(&mut self, request: &DecodeRequest) -> Result<()> {
        let frame = request.access_unit.frame;
        let key = (frame.stream, frame.generation);
        if let std::collections::hash_map::Entry::Vacant(entry) = self.decoders.entry(key) {
            let config = stream_configuration(
                request.access_unit.codec,
                [request.visible_width, request.visible_height],
                &request.access_unit.payload,
            )?;
            let (width, height) = config.extent();
            entry.insert(AndroidDecoder::new(
                &config,
                AndroidImageTarget::new(width, height, 8)?,
            )?);
        }
        Ok(())
    }
}

impl DecodeProcessor for Processor {
    type Request = Job;
    type Output = DecodedFrame<AndroidImage>;

    fn submit(&mut self, job: Job) {
        // Defer initialization and fallible admission to poll. Existing contexts
        // can accept a successor while an earlier output is still in flight.
        self.pending.push_back(Pending {
            job,
            accepted: false,
            image: None,
            deadline: None,
        });
    }

    fn poll(&mut self) -> Result<Poll<Vec<Self::Output>>> {
        let mut head = self.pending.pop_front().context("missing decoder job")?;
        let frame = head.job.frame();
        self.prepare(&head.job.0)?;
        let deadline = *head
            .deadline
            .get_or_insert_with(|| Instant::now() + Duration::from_secs(3));
        let decoder = self
            .decoders
            .get_mut(&(frame.stream, frame.generation))
            .context("decoder missing")?;
        if !head.accepted {
            head.accepted = decoder.try_send(&head.job.0.access_unit.payload, frame.sequence)?;
        }
        if head.accepted {
            for next in &mut self.pending {
                let id = next.job.frame();
                if (id.stream, id.generation) != (frame.stream, frame.generation) {
                    continue;
                }
                if !next.accepted {
                    next.accepted =
                        decoder.try_send(&next.job.0.access_unit.payload, id.sequence)?;
                    if !next.accepted {
                        break;
                    }
                }
            }
        }
        let image = loop {
            if let Some(image) = head.image.take() {
                break image;
            }
            match decoder.receive(|| false)? {
                DecodeProgress::Pending => {
                    ensure!(Instant::now() < deadline, "Android decoder timed out");
                    self.pending.push_front(head);
                    return Ok(Poll::Pending);
                }
                DecodeProgress::End => anyhow::bail!("unexpected decoder end"),
                DecodeProgress::Image(image) => {
                    let sequence = image.info().timestamp_micros;
                    if sequence == frame.sequence {
                        ensure!(head.accepted, "decoder returned an unaccepted frame");
                        break image;
                    }
                    let next = self
                        .pending
                        .iter_mut()
                        .find(|pending| {
                            let id = pending.job.frame();
                            id.stream == frame.stream
                                && id.generation == frame.generation
                                && id.sequence == sequence
                        })
                        .context("decoder output does not match an outstanding frame")?;
                    ensure!(
                        next.accepted && next.image.is_none(),
                        "duplicate or unaccepted decoder output"
                    );
                    next.image = Some(image);
                }
            }
        };
        Ok(Poll::Ready(vec![DecodedFrame {
            frame,
            buffer: image,
        }]))
    }

    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) {
        self.decoders.remove(&(stream, generation));
    }
}

/// Transfer decoded image ownership into the ordinary client buffer lifecycle.
pub struct AndroidFramePublisher;
impl DecodedFramePublisher for AndroidFramePublisher {
    type Buffer = AndroidImage;
    type ClientImporter = ();
    fn client_importer(&self) {}
    fn publish(
        &mut self,
        image: AndroidImage,
        buffer: ClientBufferId,
        use_id: ClientBufferUseId,
    ) -> Result<ClientBufferLease> {
        let info = image.info();
        Ok(ClientBufferLease::new(
            buffer,
            use_id,
            ClientBufferMetadata::new(Extent::new(info.width, info.height), true),
            Rc::new(RefCell::new(Some(image))),
            |_| {},
        )?)
    }
}
