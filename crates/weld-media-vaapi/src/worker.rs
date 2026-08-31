//! Bounded blocking workers around persistent VA-API codec sessions.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    num::NonZeroU16,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result, ensure};
use weld_media::{EncodedAccessUnit, MediaFrameId, MediaStreamId, StreamGeneration};

use crate::{H264Decoder, H264Encoder, VaapiDevice, VaapiDmabuf, VppConverter, VppOutput};

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
    pub bitrate: u64,
    pub frames_per_second: u32,
    pub intra_period: NonZeroU16,
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
    Busy(T),
    Stopped(T),
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
    pub fn spawn(render_node: PathBuf, notify: impl Fn() + Send + Sync + 'static) -> Result<Self> {
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
            return Err(VaapiWorkerSubmitError::Stopped(request));
        };
        match commands.try_send(request) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(request)) => Err(VaapiWorkerSubmitError::Busy(request)),
            Err(TrySendError::Disconnected(request)) => {
                Err(VaapiWorkerSubmitError::Stopped(request))
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
            return Err(VaapiWorkerSubmitError::Stopped(request));
        };
        match commands.try_send(request) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(request)) => Err(VaapiWorkerSubmitError::Busy(request)),
            Err(TrySendError::Disconnected(request)) => {
                Err(VaapiWorkerSubmitError::Stopped(request))
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
    width: u32,
    height: u32,
    bitrate: u64,
    frames_per_second: u32,
    intra_period: NonZeroU16,
}

fn encode_worker_loop(
    render_node: PathBuf,
    commands: Receiver<VaapiEncodeRequest>,
    completions: mpsc::Sender<VaapiEncodeCompletion>,
    notify: CompletionNotifier,
    retirements: Arc<Mutex<HashSet<GenerationKey>>>,
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
            .and_then(|(vpp, device)| encode_one(vpp, device, &mut sessions, request));
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
) -> Result<EncodedAccessUnit> {
    let key = (request.frame.stream, request.frame.generation);
    let (visible_width, visible_height) = request.input.extent();
    let coded_width = align_even(visible_width)?;
    let coded_height = align_even(visible_height)?;
    if reserve_generation(sessions, key, "VA-API encoder layer-stream limit reached")? {
        sessions.insert(
            key,
            EncoderSession {
                encoder: device.h264_encoder(
                    coded_width,
                    coded_height,
                    request.bitrate,
                    request.frames_per_second,
                    request.intra_period,
                )?,
                width: coded_width,
                height: coded_height,
                bitrate: request.bitrate,
                frames_per_second: request.frames_per_second,
                intra_period: request.intra_period,
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
    let nv12 = vpp.convert_scaled(
        &input,
        visible_width,
        visible_height,
        coded_width,
        coded_height,
        VppOutput::Nv12,
    )?;
    let session = sessions
        .get_mut(&key)
        .context("VA-API encoder session disappeared")?;
    ensure!(
        (session.width, session.height) == (coded_width, coded_height)
            && session.bitrate == request.bitrate
            && session.frames_per_second == request.frames_per_second
            && session.intra_period == request.intra_period,
        "encoded stream generation configuration changed without retirement"
    );
    match session
        .encoder
        .encode(request.frame, request.timestamp_micros, &nv12)
    {
        Ok(frame) => Ok(frame),
        Err(error) => {
            sessions.remove(&key);
            Err(error)
        }
    }
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
            let dmabuf = vpp.convert_scaled(
                &decoded.frame,
                decoded.display_width,
                decoded.display_height,
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

fn align_even(value: u32) -> Result<u32> {
    ensure!(value > 0, "encoded frame has zero extent");
    value
        .checked_add(value % 2)
        .context("encoded frame extent overflow")
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
