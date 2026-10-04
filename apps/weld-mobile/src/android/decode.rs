//! Android images produced on Weld's bounded, stream-affine decode workers.
use std::{
    collections::HashMap,
    task::Poll,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use weld_hoist_encoded::{
    DecodeBackend, DecodeCompletion, DecodeRequest, DecodedFrame, SubmitError,
};
use weld_media::{
    MediaFrameId, MediaStreamId, StreamGeneration, VideoCodec, WorkerSubmitError,
    decode::{DecodeJob, DecodePool, DecodePoolLimits, DecodeProcessor},
};
use weld_media_android::{
    AndroidDecoder, AndroidImage, AndroidImageTarget, DecodeProgress, stream_configuration,
};

pub(super) struct Backend {
    pool: DecodePool<Processor>,
    codec: VideoCodec,
}

impl Backend {
    pub fn new(codec: VideoCodec) -> Result<Self> {
        let owner = thread::current();
        Ok(Self {
            // The first phone proof admits one outstanding job per worker.
            // Shared pool machinery owns scheduling, wakeups and retirement.
            pool: DecodePool::new(
                DecodePoolLimits::try_new(2, 2, 8, 1)?,
                || Ok(Processor::default()),
                move || owner.unpark(),
            ),
            codec,
        })
    }
}

impl DecodeBackend for Backend {
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
    deadline: Instant,
}
#[derive(Default)]
struct Processor {
    decoders: HashMap<(MediaStreamId, StreamGeneration), AndroidDecoder>,
    pending: Option<Pending>,
}
impl DecodeProcessor for Processor {
    type Request = Job;
    type Output = DecodedFrame<AndroidImage>;
    fn submit(&mut self, job: Job) {
        self.pending = Some(Pending {
            job,
            accepted: false,
            deadline: Instant::now() + Duration::from_secs(3),
        });
    }
    fn poll(&mut self) -> Result<Poll<Vec<Self::Output>>> {
        let pending = self.pending.as_mut().context("missing decoder job")?;
        ensure!(
            Instant::now() < pending.deadline,
            "Android decoder timed out"
        );
        let request = &pending.job.0;
        let frame = request.access_unit.frame;
        let key = (frame.stream, frame.generation);
        if let std::collections::hash_map::Entry::Vacant(entry) = self.decoders.entry(key) {
            let config = stream_configuration(
                request.access_unit.codec,
                [request.visible_width, request.visible_height],
                &request.access_unit.payload,
            )?;
            let (width, height) = config.extent();
            ensure!(
                width <= 4096 && height <= 4096,
                "phone decoder extent exceeds proof limit"
            );
            entry.insert(AndroidDecoder::new(
                &config,
                AndroidImageTarget::new(width, height, 8)?,
            )?);
        }
        let decoder = self.decoders.get_mut(&key).context("decoder missing")?;
        if !pending.accepted {
            pending.accepted = decoder.try_send(&request.access_unit.payload, frame.sequence)?;
        }
        match decoder.receive(|| false)? {
            DecodeProgress::Pending => Ok(Poll::Pending),
            DecodeProgress::End => anyhow::bail!("unexpected decoder end"),
            DecodeProgress::Image(image) => {
                ensure!(
                    pending.accepted && image.info().timestamp_micros == frame.sequence,
                    "decoder output does not match its admitted frame"
                );
                self.pending = None;
                Ok(Poll::Ready(vec![DecodedFrame {
                    frame,
                    buffer: image,
                }]))
            }
        }
    }
    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) {
        self.decoders.remove(&(stream, generation));
    }
}
