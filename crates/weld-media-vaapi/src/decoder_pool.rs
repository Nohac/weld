//! FFmpeg/VA-API processor and native configuration for the shared decode pool.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
};

use anyhow::{Context, Result, ensure};
use weld_media::{
    EncodedAccessUnit, MediaFrameId, MediaStreamId, StreamGeneration, VideoCodec,
    decode::{DecodeCompletion, DecodeJob, DecodePool, DecodeProcessor},
};

use crate::{
    FfmpegDecoder, FfmpegVaapiDevice, PendingDecodedFrame, VaapiDevice, VaapiDmabuf,
    VaapiWorkerSubmitError, VppConverter,
};
pub use weld_media::decode::DecodePoolLimits;

type GenerationKey = (MediaStreamId, StreamGeneration);

pub struct VaapiDecodeRequest {
    pub token: u64,
    pub access_unit: EncodedAccessUnit,
    pub visible_width: u32,
    pub visible_height: u32,
    pub xrgb_modifiers: Vec<u64>,
}

pub struct VaapiDecodedFrame {
    pub frame: MediaFrameId,
    pub dmabuf: VaapiDmabuf,
}

/// Completion from the shared pool, retaining VA-API's native output type.
pub type VaapiDecodeCompletion = DecodeCompletion<VaapiDecodedFrame>;

impl DecodeJob for VaapiDecodeRequest {
    fn token(&self) -> u64 {
        self.token
    }
    fn frame(&self) -> MediaFrameId {
        self.access_unit.frame
    }
}

/// Native configuration facade; scheduling and retirement live in [`DecodePool`].
pub struct VaapiDecodeWorker {
    pool: DecodePool<NativeProcessor>,
}

impl VaapiDecodeWorker {
    pub fn spawn(render_node: PathBuf, notify: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        Self::with_limits(render_node, DecodePoolLimits::default(), notify)
    }

    pub fn with_limits(
        render_node: PathBuf,
        limits: DecodePoolLimits,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        Ok(Self {
            pool: DecodePool::new(
                limits,
                move || {
                    let device = VaapiDevice::open(&render_node)?;
                    Ok(NativeProcessor {
                        vpp: device.vpp_converter()?,
                        device: FfmpegVaapiDevice::open(&render_node)?,
                        sessions: HashMap::new(),
                        pending: VecDeque::new(),
                        poisoned: HashMap::new(),
                        depth: limits.depth(),
                    })
                },
                notify,
            ),
        })
    }

    pub fn try_decode(
        &mut self,
        request: VaapiDecodeRequest,
    ) -> Result<(), VaapiWorkerSubmitError<VaapiDecodeRequest>> {
        self.pool.try_decode(request)
    }

    pub fn drain(&mut self) -> (Vec<VaapiDecodeCompletion>, Option<anyhow::Error>) {
        self.pool.drain()
    }

    pub fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()> {
        self.pool.retire(stream, generation)
    }
}

struct NativeProcessor {
    // Hardware frames are released before the codec/device owners on shutdown.
    pending: VecDeque<NativePending>,
    poisoned: HashMap<GenerationKey, String>,
    sessions: HashMap<GenerationKey, NativeSession>,
    vpp: VppConverter,
    device: FfmpegVaapiDevice,
    depth: usize,
}

struct NativePending {
    key: GenerationKey,
    frame: MediaFrameId,
    width: u32,
    height: u32,
    modifiers: Vec<u64>,
    result: Result<PendingDecodedFrame>,
}

struct NativeSession {
    decoder: FfmpegDecoder,
    codec: VideoCodec,
}

impl NativeProcessor {
    fn prepare(&mut self, request: &VaapiDecodeRequest) -> Result<PendingDecodedFrame> {
        let frame = request.access_unit.frame;
        let key = (frame.stream, frame.generation);
        if let Some(error) = self.poisoned.get(&key) {
            anyhow::bail!(error.clone());
        }
        if let std::collections::hash_map::Entry::Vacant(entry) = self.sessions.entry(key) {
            entry.insert(NativeSession {
                decoder: FfmpegDecoder::new(request.access_unit.codec, &self.device, self.depth)?,
                codec: request.access_unit.codec,
            });
        }
        let session = self
            .sessions
            .get_mut(&key)
            .context("decoder session disappeared")?;
        ensure!(
            session.codec == request.access_unit.codec,
            "encoded stream generation changed codec without retirement"
        );
        session.decoder.submit(
            &request.access_unit.payload,
            request.access_unit.timestamp_micros,
        )
    }
}

impl DecodeProcessor for NativeProcessor {
    type Request = VaapiDecodeRequest;
    type Output = VaapiDecodedFrame;

    fn submit(&mut self, request: VaapiDecodeRequest) {
        let frame = request.access_unit.frame;
        let key = (frame.stream, frame.generation);
        let result = self.prepare(&request);
        if let Err(error) = &result {
            self.poisoned
                .entry(key)
                .or_insert_with(|| format!("{error:#}"));
        }
        self.pending.push_back(NativePending {
            key,
            frame,
            width: request.visible_width,
            height: request.visible_height,
            modifiers: request.xrgb_modifiers,
            result,
        });
    }

    fn complete(&mut self) -> Result<Vec<VaapiDecodedFrame>> {
        let pending = self
            .pending
            .pop_front()
            .context("native decoder FIFO is empty")?;
        let result = pending.result.and_then(|frame| {
            let decoded =
                frame.finish(pending.width, pending.height, &pending.modifiers, &self.vpp)?;
            ensure!(
                decoded.dmabuf.width == pending.width && decoded.dmabuf.height == pending.height,
                "decoded output extent differs from transported visible extent"
            );
            Ok(vec![VaapiDecodedFrame {
                frame: pending.frame,
                dmabuf: decoded.dmabuf,
            }])
        });
        if let Err(error) = &result {
            self.poisoned
                .entry(pending.key)
                .or_insert_with(|| format!("{error:#}"));
        }
        if self.poisoned.contains_key(&pending.key)
            && !self.pending.iter().any(|queued| queued.key == pending.key)
        {
            self.sessions.remove(&pending.key);
            self.poisoned.remove(&pending.key);
        }
        result
    }

    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) {
        let key = (stream, generation);
        self.sessions.remove(&key);
        self.poisoned.remove(&key);
    }
}
