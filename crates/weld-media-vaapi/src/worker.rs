//! Bounded blocking workers around persistent VA-API codec sessions.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use anyhow::{Context, Result, ensure};
use weld_media::{EncodedAccessUnit, MediaFrameId, MediaStreamId, StreamGeneration};

use crate::{
    FfmpegEncodeDevice, FfmpegEncoder, VaapiDevice, VaapiDmabuf, VaapiEncodeGeometry,
    VaapiEncoderSettings, VaapiWorkerSubmitError, VppConverter,
};

const WORK_QUEUE_CAPACITY: usize = 1;
// The encoded tracer assigns one persistent codec stream per surface-tree layer.
// Sixteen covers the observed Firefox and Blender trees while keeping VA context
// ownership explicitly bounded until capability-driven budgeting exists.
const MAX_ACTIVE_GENERATIONS: usize = 16;

type GenerationKey = (MediaStreamId, StreamGeneration);
type CompletionNotifier = Arc<dyn Fn() + Send + Sync>;

/// Owned source pixels accepted by the hardware encoder worker.
pub enum VaapiEncodeInput {
    Dmabuf(VaapiDmabuf),
    PackedBgra {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
}

impl VaapiEncodeInput {
    fn extent(&self) -> (u32, u32) {
        match self {
            Self::Dmabuf(frame) => (frame.width, frame.height),
            Self::PackedBgra { width, height, .. } => (*width, *height),
        }
    }
}

pub struct VaapiEncodeRequest {
    pub token: u64,
    pub frame: MediaFrameId,
    pub timestamp_micros: u64,
    pub settings: VaapiEncoderSettings,
    pub input: VaapiEncodeInput,
}

pub struct VaapiEncodeCompletion {
    pub token: u64,
    pub result: Result<EncodedAccessUnit>,
}

pub struct VaapiEncodeWorker {
    commands: Option<SyncSender<VaapiEncodeRequest>>,
    retirements: Arc<Mutex<HashSet<GenerationKey>>>,
    completions: Receiver<VaapiEncodeCompletion>,
    thread: Option<JoinHandle<()>>,
}

impl VaapiEncodeWorker {
    pub fn spawn(
        render_node: PathBuf,
        dump_directory: Option<PathBuf>,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        let (command_sender, command_receiver) = mpsc::sync_channel(WORK_QUEUE_CAPACITY);
        let (completion_sender, completions) = mpsc::channel();
        let retirements = Arc::new(Mutex::new(HashSet::new()));
        let worker_retirements = retirements.clone();
        let notifier: CompletionNotifier = Arc::new(notify);
        let thread = thread::Builder::new()
            .name("weld-vaapi-encode".to_owned())
            .spawn(move || {
                encode_worker_loop(
                    render_node,
                    command_receiver,
                    completion_sender,
                    notifier,
                    worker_retirements,
                    dump_directory,
                );
            })
            .context("could not spawn VA-API encoder worker")?;
        Ok(Self {
            commands: Some(command_sender),
            retirements,
            completions,
            thread: Some(thread),
        })
    }

    pub fn try_encode(
        &self,
        request: VaapiEncodeRequest,
    ) -> Result<(), VaapiWorkerSubmitError<VaapiEncodeRequest>> {
        let Some(commands) = &self.commands else {
            return Err(VaapiWorkerSubmitError::Stopped(Box::new(request)));
        };
        match commands.try_send(request) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(request)) => {
                Err(VaapiWorkerSubmitError::Busy(Box::new(request)))
            }
            Err(TrySendError::Disconnected(request)) => {
                Err(VaapiWorkerSubmitError::Stopped(Box::new(request)))
            }
        }
    }

    pub fn try_retire(&self, stream: MediaStreamId, generation: StreamGeneration) -> bool {
        self.retirements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((stream, generation));
        self.commands.is_some()
    }

    pub fn drain(&self) -> impl Iterator<Item = VaapiEncodeCompletion> + '_ {
        self.completions.try_iter()
    }
}

impl Drop for VaapiEncodeWorker {
    fn drop(&mut self) {
        self.commands.take();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("VA-API encoder worker panicked");
        }
    }
}

struct EncoderSession {
    encoder: FfmpegEncoder,
    visible_size: (u32, u32),
    coded_size: (u32, u32),
    settings: VaapiEncoderSettings,
}

#[derive(Default)]
struct EncodeGeometryCache {
    h264: Option<VaapiEncodeGeometry>,
    av1: Option<VaapiEncodeGeometry>,
}

impl EncodeGeometryCache {
    fn get(
        &mut self,
        device: &VaapiDevice,
        codec: weld_media::VideoCodec,
    ) -> Result<VaapiEncodeGeometry> {
        let cached = match codec {
            weld_media::VideoCodec::H264 => &mut self.h264,
            weld_media::VideoCodec::Av1 => &mut self.av1,
            weld_media::VideoCodec::Vp9 => anyhow::bail!("VP9 VA-API encoding is not implemented"),
        };
        if let Some(geometry) = *cached {
            return Ok(geometry);
        }
        let geometry = device.encode_geometry(codec)?;
        *cached = Some(geometry);
        Ok(geometry)
    }
}

fn encode_worker_loop(
    render_node: PathBuf,
    commands: Receiver<VaapiEncodeRequest>,
    completions: mpsc::Sender<VaapiEncodeCompletion>,
    notify: CompletionNotifier,
    retirements: Arc<Mutex<HashSet<GenerationKey>>>,
    dump_directory: Option<PathBuf>,
) {
    let runtime = VaapiDevice::open(&render_node).and_then(|device| {
        let vpp = device.vpp_converter()?;
        let ffmpeg = FfmpegEncodeDevice::open(&render_node)?;
        Ok((device, vpp, ffmpeg))
    });
    let mut sessions = HashMap::<GenerationKey, EncoderSession>::new();
    let mut geometries = EncodeGeometryCache::default();
    while let Ok(request) = commands.recv() {
        apply_retirements(&retirements, &mut sessions);
        let token = request.token;
        let result = runtime
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.to_string()))
            .and_then(|(device, vpp, ffmpeg_device)| {
                encode_one(
                    device,
                    vpp,
                    ffmpeg_device,
                    &mut geometries,
                    &mut sessions,
                    request,
                    dump_directory.as_deref(),
                )
            });
        if completions
            .send(VaapiEncodeCompletion { token, result })
            .is_err()
        {
            break;
        }
        notify();
    }
}

fn encode_one(
    device: &VaapiDevice,
    vpp: &VppConverter,
    ffmpeg_device: &FfmpegEncodeDevice,
    geometries: &mut EncodeGeometryCache,
    sessions: &mut HashMap<GenerationKey, EncoderSession>,
    request: VaapiEncodeRequest,
    dump_directory: Option<&std::path::Path>,
) -> Result<EncodedAccessUnit> {
    let key = (request.frame.stream, request.frame.generation);
    let (visible_width, visible_height) = request.input.extent();
    let visible_size = (visible_width, visible_height);
    if reserve_generation(sessions, key, "VA-API encoder layer-stream limit reached")? {
        let geometry = geometries.get(device, request.settings.codec())?;
        let coded_size = geometry.coded_extent(visible_width, visible_height)?;
        let encoder = FfmpegEncoder::new(
            request.settings,
            geometry,
            ffmpeg_device,
            visible_width,
            visible_height,
        )?;
        sessions.insert(
            key,
            EncoderSession {
                encoder,
                visible_size,
                coded_size,
                settings: request.settings,
            },
        );
    }
    let input = match request.input {
        VaapiEncodeInput::Dmabuf(frame) => frame,
        VaapiEncodeInput::PackedBgra {
            width,
            height,
            pixels,
        } => vpp.upload_bgra(width, height, &pixels)?,
    };
    let active_sessions = sessions.len();
    let result = {
        let session = sessions
            .get_mut(&key)
            .context("VA-API encoder session disappeared")?;
        ensure!(
            session.visible_size == visible_size && session.settings == request.settings,
            "encoded stream generation configuration changed without retirement"
        );
        if should_dump_stages(request.frame)
            && let Some(directory) = dump_directory
            && let Err(error) =
                dump_encode_source(vpp, directory, request.frame, &input, visible_size)
        {
            tracing::warn!(%error, frame = ?request.frame, "could not record VA-API encode stages");
        }
        let started_at = Instant::now();
        let packet = session.encoder.encode(input, request.timestamp_micros)?;
        tracing::trace!(
            stream = request.frame.stream.raw(),
            generation = request.frame.generation.raw(),
            sequence = request.frame.sequence,
            codec = ?request.settings.codec(),
            visible_width,
            visible_height,
            coded_width = session.coded_size.0,
            coded_height = session.coded_size.1,
            bitrate_bits = request.settings.bitrate_bits(),
            active_sessions,
            payload_bytes = packet.payload.len(),
            encode_micros = started_at.elapsed().as_micros(),
            "encoded VA-API frame"
        );
        ensure!(
            packet.timestamp_micros == request.timestamp_micros,
            "FFmpeg encoder changed the submitted frame timestamp"
        );
        Ok(EncodedAccessUnit {
            frame: request.frame,
            codec: request.settings.codec(),
            kind: packet.kind,
            timestamp_micros: packet.timestamp_micros,
            payload: packet.payload,
        })
    };
    if result.is_err() {
        sessions.remove(&key);
    }
    result
}

fn should_dump_stages(frame: MediaFrameId) -> bool {
    frame.sequence <= 300 && frame.sequence.is_multiple_of(30)
}

fn dump_encode_source(
    vpp: &VppConverter,
    directory: &std::path::Path,
    frame: MediaFrameId,
    source: &VaapiDmabuf,
    visible_size: (u32, u32),
) -> Result<()> {
    let directory = directory.join("stages");
    std::fs::create_dir_all(&directory).with_context(|| {
        format!(
            "could not create stage dump directory {}",
            directory.display()
        )
    })?;
    let stem = format!(
        "stream-{}-generation-{}-sequence-{}",
        frame.stream.raw(),
        frame.generation.raw(),
        frame.sequence
    );
    vpp.write_xrgb_ppm(
        source,
        visible_size.0,
        visible_size.1,
        &directory.join(format!("{stem}-source.ppm")),
    )?;
    Ok(())
}

fn apply_retirements<T>(
    retirements: &Mutex<HashSet<GenerationKey>>,
    sessions: &mut HashMap<GenerationKey, T>,
) {
    let mut retirements = retirements
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for key in retirements.drain() {
        sessions.remove(&key);
    }
}

fn reserve_generation<T>(
    sessions: &mut HashMap<GenerationKey, T>,
    key: GenerationKey,
    limit_message: &'static str,
) -> Result<bool> {
    if sessions.contains_key(&key) {
        return Ok(false);
    }
    sessions.retain(|(stream, _), _| *stream != key.0);
    ensure!(sessions.len() < MAX_ACTIVE_GENERATIONS, limit_message);
    Ok(true)
}

#[cfg(test)]
mod worker_policy_tests {
    use super::*;

    #[test]
    fn generation_reservation_replaces_one_stream_but_rejects_another() {
        let mut sessions = (0..MAX_ACTIVE_GENERATIONS)
            .map(|stream| {
                (
                    (
                        MediaStreamId::new(u64::try_from(stream).expect("stream index") + 1),
                        StreamGeneration::new(1),
                    ),
                    (),
                )
            })
            .collect::<HashMap<_, _>>();

        assert!(
            reserve_generation(
                &mut sessions,
                (MediaStreamId::new(1), StreamGeneration::new(2)),
                "limit",
            )
            .expect("same-stream replacement")
        );
        sessions.insert((MediaStreamId::new(1), StreamGeneration::new(2)), ());
        assert!(
            reserve_generation(
                &mut sessions,
                (MediaStreamId::new(99), StreamGeneration::new(1)),
                "limit",
            )
            .is_err()
        );
    }
}
