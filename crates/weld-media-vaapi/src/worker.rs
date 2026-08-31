//! Bounded blocking workers around persistent VA-API codec sessions.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result, ensure};
use weld_media::{EncodedAccessUnit, MediaFrameId, MediaStreamId, StreamGeneration};

use crate::{
    H264Decoder, H264Encoder, H264EncoderSettings, H264ReferenceMode, VaapiDevice, VaapiDmabuf,
    VppConverter, VppOutput,
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
    pub settings: H264EncoderSettings,
    pub input: VaapiEncodeInput,
}

pub struct VaapiEncodeCompletion {
    pub token: u64,
    pub result: Result<EncodedAccessUnit>,
}

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

pub struct VaapiDecodeCompletion {
    pub token: u64,
    pub result: Result<Vec<VaapiDecodedFrame>>,
}

/// A bounded worker rejected work without consuming it.
pub enum VaapiWorkerSubmitError<T> {
    Busy(Box<T>),
    Stopped(Box<T>),
}

impl<T> fmt::Debug for VaapiWorkerSubmitError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy(_) => formatter.write_str("VaapiWorkerSubmitError::Busy"),
            Self::Stopped(_) => formatter.write_str("VaapiWorkerSubmitError::Stopped"),
        }
    }
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

pub struct VaapiDecodeWorker {
    commands: Option<SyncSender<VaapiDecodeRequest>>,
    retirements: Arc<Mutex<HashSet<GenerationKey>>>,
    completions: Receiver<VaapiDecodeCompletion>,
    thread: Option<JoinHandle<()>>,
}

impl VaapiDecodeWorker {
    pub fn spawn(render_node: PathBuf, notify: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        let (command_sender, command_receiver) = mpsc::sync_channel(WORK_QUEUE_CAPACITY);
        let (completion_sender, completions) = mpsc::channel();
        let retirements = Arc::new(Mutex::new(HashSet::new()));
        let worker_retirements = retirements.clone();
        let notifier: CompletionNotifier = Arc::new(notify);
        let thread = thread::Builder::new()
            .name("weld-vaapi-decode".to_owned())
            .spawn(move || {
                decode_worker_loop(
                    render_node,
                    command_receiver,
                    completion_sender,
                    notifier,
                    worker_retirements,
                );
            })
            .context("could not spawn VA-API decoder worker")?;
        Ok(Self {
            commands: Some(command_sender),
            retirements,
            completions,
            thread: Some(thread),
        })
    }

    pub fn try_decode(
        &self,
        request: VaapiDecodeRequest,
    ) -> Result<(), VaapiWorkerSubmitError<VaapiDecodeRequest>> {
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

    pub fn drain(&self) -> impl Iterator<Item = VaapiDecodeCompletion> + '_ {
        self.completions.try_iter()
    }
}

impl Drop for VaapiDecodeWorker {
    fn drop(&mut self) {
        self.commands.take();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("VA-API decoder worker panicked");
        }
    }
}

struct EncoderSession {
    encoder: H264Encoder,
    visible_size: (u32, u32),
    coded_size: (u32, u32),
    settings: H264EncoderSettings,
}

fn encode_worker_loop(
    render_node: PathBuf,
    commands: Receiver<VaapiEncodeRequest>,
    completions: mpsc::Sender<VaapiEncodeCompletion>,
    notify: CompletionNotifier,
    retirements: Arc<Mutex<HashSet<GenerationKey>>>,
    dump_directory: Option<PathBuf>,
) {
    let runtime =
        VaapiDevice::open(render_node).and_then(|device| Ok((device.vpp_converter()?, device)));
    let mut sessions = HashMap::<GenerationKey, EncoderSession>::new();
    while let Ok(request) = commands.recv() {
        apply_retirements(&retirements, &mut sessions);
        let token = request.token;
        let result = runtime
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.to_string()))
            .and_then(|(vpp, device)| {
                encode_one(
                    vpp,
                    device,
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
    vpp: &VppConverter,
    device: &VaapiDevice,
    sessions: &mut HashMap<GenerationKey, EncoderSession>,
    request: VaapiEncodeRequest,
    dump_directory: Option<&std::path::Path>,
) -> Result<EncodedAccessUnit> {
    let key = (request.frame.stream, request.frame.generation);
    let (visible_width, visible_height) = request.input.extent();
    let visible_size = (visible_width, visible_height);
    let independent_idr = request.settings.reference_mode() == H264ReferenceMode::IndependentIdr;
    // Current cros-codecs cannot express an all-IDR predictor: force_keyframe
    // emits a non-IDR I slice without parameter sets, while period 1
    // underflows SPS frame-number derivation. Independent mode therefore
    // recreates the session so every frame starts at counter zero with SPS/PPS.
    if reserve_encoder_generation(
        sessions,
        key,
        request.settings.reference_mode(),
        "VA-API encoder layer-stream limit reached",
    )? {
        let encoder = device.h264_encoder(visible_width, visible_height, request.settings)?;
        let coded_size = encoder.coded_size();
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
    let result = {
        let session = sessions
            .get_mut(&key)
            .context("VA-API encoder session disappeared")?;
        ensure!(
            session.visible_size == visible_size && session.settings == request.settings,
            "encoded stream generation configuration changed without retirement"
        );
        let nv12 = vpp.convert_padded(
            &input,
            visible_width,
            visible_height,
            session.coded_size.0,
            session.coded_size.1,
            VppOutput::Nv12,
        )?;
        if should_dump_stages(request.frame)
            && let Some(directory) = dump_directory
            && let Err(error) =
                dump_encode_stages(vpp, directory, request.frame, &input, &nv12, visible_size)
        {
            tracing::warn!(%error, frame = ?request.frame, "could not record VA-API encode stages");
        }
        session
            .encoder
            .encode(request.frame, request.timestamp_micros, &nv12)
    };
    if independent_idr || result.is_err() {
        sessions.remove(&key);
    }
    result
}

fn should_dump_stages(frame: MediaFrameId) -> bool {
    frame.sequence <= 300 && frame.sequence.is_multiple_of(30)
}

fn dump_encode_stages(
    vpp: &VppConverter,
    directory: &std::path::Path,
    frame: MediaFrameId,
    source: &VaapiDmabuf,
    normalized: &VaapiDmabuf,
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
    vpp.write_xrgb_ppm(
        normalized,
        visible_size.0,
        visible_size.1,
        &directory.join(format!("{stem}-normalized.ppm")),
    )?;
    Ok(())
}

struct DecoderSession {
    decoder: H264Decoder,
    pending: HashMap<u64, PendingDecodedFrame>,
}

struct PendingDecodedFrame {
    frame: MediaFrameId,
    visible_width: u32,
    visible_height: u32,
    xrgb_modifiers: Vec<u64>,
}

fn decode_worker_loop(
    render_node: PathBuf,
    commands: Receiver<VaapiDecodeRequest>,
    completions: mpsc::Sender<VaapiDecodeCompletion>,
    notify: CompletionNotifier,
    retirements: Arc<Mutex<HashSet<GenerationKey>>>,
) {
    let runtime =
        VaapiDevice::open(render_node).and_then(|device| Ok((device.vpp_converter()?, device)));
    let mut sessions = HashMap::<GenerationKey, DecoderSession>::new();
    while let Ok(request) = commands.recv() {
        apply_retirements(&retirements, &mut sessions);
        let token = request.token;
        let result = runtime
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.to_string()))
            .and_then(|(vpp, device)| decode_one(vpp, device, &mut sessions, request));
        if completions
            .send(VaapiDecodeCompletion { token, result })
            .is_err()
        {
            break;
        }
        notify();
    }
}

fn decode_one(
    vpp: &VppConverter,
    device: &VaapiDevice,
    sessions: &mut HashMap<GenerationKey, DecoderSession>,
    request: VaapiDecodeRequest,
) -> Result<Vec<VaapiDecodedFrame>> {
    let key = (
        request.access_unit.frame.stream,
        request.access_unit.frame.generation,
    );
    if reserve_generation(sessions, key, "VA-API decoder layer-stream limit reached")? {
        sessions.insert(
            key,
            DecoderSession {
                decoder: device.h264_decoder()?,
                pending: HashMap::new(),
            },
        );
    }
    let session = sessions
        .get_mut(&key)
        .context("VA-API decoder session disappeared")?;
    ensure!(
        session
            .pending
            .insert(
                request.access_unit.timestamp_micros,
                PendingDecodedFrame {
                    frame: request.access_unit.frame,
                    visible_width: request.visible_width,
                    visible_height: request.visible_height,
                    xrgb_modifiers: request.xrgb_modifiers,
                },
            )
            .is_none(),
        "encoded frame timestamp was reused within one generation"
    );
    let decoded = match session.decoder.decode(&request.access_unit) {
        Ok(decoded) => decoded,
        Err(error) => {
            sessions.remove(&key);
            return Err(error);
        }
    };
    decoded
        .into_iter()
        .map(|decoded| {
            let pending = session
                .pending
                .remove(&decoded.timestamp_micros)
                .context("decoder returned an unknown frame timestamp")?;
            ensure!(
                pending.visible_width <= decoded.display_width
                    && pending.visible_height <= decoded.display_height,
                "decoded H.264 display extent is smaller than its transported visible extent"
            );
            let dmabuf = vpp.convert_scaled(
                &decoded.frame,
                pending.visible_width,
                pending.visible_height,
                pending.visible_width,
                pending.visible_height,
                VppOutput::Xrgb8888 {
                    modifiers: pending.xrgb_modifiers,
                },
            )?;
            Ok(VaapiDecodedFrame {
                frame: pending.frame,
                dmabuf,
            })
        })
        .collect()
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

fn reserve_encoder_generation<T>(
    sessions: &mut HashMap<GenerationKey, T>,
    key: GenerationKey,
    reference_mode: H264ReferenceMode,
    limit_message: &'static str,
) -> Result<bool> {
    if reference_mode == H264ReferenceMode::IndependentIdr {
        sessions.remove(&key);
    }
    reserve_generation(sessions, key, limit_message)
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

    #[test]
    fn independent_idr_recreates_a_generation_that_low_delay_reuses() {
        let key = (MediaStreamId::new(1), StreamGeneration::new(1));
        let mut sessions = HashMap::from([(key, ())]);

        assert!(
            !reserve_encoder_generation(&mut sessions, key, H264ReferenceMode::LowDelay, "limit",)
                .expect("reuse persistent generation")
        );
        assert!(sessions.contains_key(&key));
        assert!(
            reserve_encoder_generation(
                &mut sessions,
                key,
                H264ReferenceMode::IndependentIdr,
                "limit",
            )
            .expect("recreate independent generation")
        );
        assert!(!sessions.contains_key(&key));
    }
}
