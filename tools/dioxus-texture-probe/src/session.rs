//! One approved live source, using Weld's receiver and bounded presentation handoff.
use crate::platform;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId,
    ClientPresentationInbox, ClientRequest, ClientRuntime, ClientSourceId, ClientSurfaceEventKind,
    ClientSurfaceRequest, ClientSurfaceRequestKind, ClientSurfaceRole, Extent, PresentationMailbox,
    PresentationRate, SurfaceBufferChange, SurfaceStreamMode, ToplevelLayout,
};
use weld_hoist_encoded::{
    DecodeBackend, DecodeCompletion, DecodeRequest, DecodedFrame, DecodedFramePublisher,
    EncodedDestinationTransport, SubmitError,
};
use weld_hoist_iroh::{
    IrohConnectionProfile, IrohDestinationPeer, IrohDeviceIdentity, IrohHost, IrohNotifier,
    IrohPeerIdentity, IrohReceiverPreferences, destination_registration_with_backend,
};
use weld_media::{MediaStreamId, StreamGeneration, VideoCodec};

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuePolicy {
    #[default]
    Smoothing,
    Latest,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    #[serde(default)]
    pub queue: QueuePolicy,
    #[serde(default = "default_nonblocking_poll")]
    pub nonblocking_poll: bool,
}
fn default_nonblocking_poll() -> bool {
    true
}
impl Settings {
    pub fn load(directory: &std::path::Path) -> Result<Self> {
        let value: Self = serde_json::from_slice(&std::fs::read(directory.join("live.json"))?)?;
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=4096).contains(&self.width) && (1..=4096).contains(&self.height),
            "probe extent must be 1..4096"
        );
        ensure!((1..=120).contains(&self.fps), "probe rate must be 1..120");
        Ok(())
    }
    pub fn rate(&self) -> PresentationRate {
        PresentationRate::try_from(self.fps * 1000).unwrap_or(PresentationRate::HZ_60)
    }
}

pub struct Frame {
    pub image: platform::Image,
    pub ready: Instant,
    pub decoded_at: Option<Instant>,
}
struct TimedImage {
    image: platform::Image,
    decoded_at: Option<Instant>,
}
struct TimedBackend(Box<dyn DecodeBackend<Output = platform::Image>>);
impl DecodeBackend for TimedBackend {
    type Output = TimedImage;
    fn try_submit(&mut self, request: DecodeRequest) -> Result<(), SubmitError<DecodeRequest>> {
        self.0.try_submit(request)
    }
    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()> {
        self.0.retire(stream, generation)
    }
    fn drain(&mut self) -> (Vec<DecodeCompletion<TimedImage>>, Option<anyhow::Error>) {
        let (items, error) = self.0.drain();
        (
            items
                .into_iter()
                .map(|item| {
                    let decoded_at = item.timing.as_ref().map(|timing| timing.completed_at);
                    DecodeCompletion {
                        token: item.token,
                        timing: item.timing,
                        result: item.result.map(|frames| {
                            frames
                                .into_iter()
                                .map(|frame| DecodedFrame {
                                    frame: frame.frame,
                                    buffer: TimedImage {
                                        image: frame.buffer,
                                        decoded_at,
                                    },
                                })
                                .collect()
                        }),
                    }
                })
                .collect(),
            error,
        )
    }
}
pub struct Shared {
    redraw: Arc<dyn Fn() + Send + Sync>,
    pub wake: Mutex<crate::timing::Wake>,
    pub frames: Mutex<PresentationMailbox<Frame>>,
    pub status: Mutex<String>,
    pub stopped: AtomicBool,
    pub active: AtomicBool,
    pub paused: AtomicBool,
    pub clear: AtomicBool,
}
impl Shared {
    pub fn followup_redraw(&self) {
        (self.redraw)();
    }
    pub fn request_redraw(&self) {
        if let Ok(mut wake) = self.wake.lock() {
            wake.request(Instant::now());
        }
        (self.redraw)();
    }
    pub fn message(&self, message: impl Into<String>) {
        let message = message.into();
        log::info!("{message}");
        if let Ok(mut status) = self.status.lock() {
            *status = message;
        }
        self.request_redraw();
    }
}
pub struct Session {
    pub shared: Arc<Shared>,
    worker: thread::JoinHandle<()>,
}
impl Session {
    pub fn start(
        directory: PathBuf,
        settings: Settings,
        redraw: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        let shared = Arc::new(Shared {
            redraw,
            wake: Mutex::new(crate::timing::Wake::default()),
            frames: Mutex::new(match settings.queue {
                QueuePolicy::Smoothing => {
                    PresentationMailbox::smoothing(settings.rate().interval())
                }
                QueuePolicy::Latest => PresentationMailbox::default(),
            }),
            status: Mutex::new("Waiting for source profile".into()),
            stopped: AtomicBool::new(false),
            active: AtomicBool::new(true),
            paused: AtomicBool::new(false),
            clear: AtomicBool::new(false),
        });
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("dioxus-receiver".into())
            .spawn(move || {
                if let Err(error) = receive(&state, directory, settings) {
                    state.message(format!("Receiver stopped: {error:#}"));
                }
                state.active.store(false, Ordering::Release);
                state.stopped.store(true, Ordering::Release);
                if let Ok(mut frames) = state.frames.lock() {
                    drop(frames.drain());
                }
                state.clear.store(true, Ordering::Release);
                state.request_redraw();
            })?;
        Ok(Self { shared, worker })
    }
    pub fn wake(&self) {
        self.worker.thread().unpark();
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        self.wake();
    }
}
struct Connection(IrohDestinationPeer);
impl Drop for Connection {
    fn drop(&mut self) {
        self.0.disconnect();
    }
}
struct Publisher;
impl DecodedFramePublisher for Publisher {
    type Buffer = TimedImage;
    type ClientImporter = ();
    fn client_importer(&self) {}
    fn publish(
        &mut self,
        image: Self::Buffer,
        id: ClientBufferId,
        use_id: ClientBufferUseId,
    ) -> Result<ClientBufferLease> {
        let [width, height] = platform::extent(&image.image);
        Ok(ClientBufferLease::new(
            id,
            use_id,
            ClientBufferMetadata::new(Extent::new(width, height), true),
            Rc::new(RefCell::new(Some(image))),
            |_| {},
        )?)
    }
}
fn receive(shared: &Shared, directory: PathBuf, settings: Settings) -> Result<()> {
    log::info!("probe queue policy: {:?}", settings.queue);
    let identity = IrohDeviceIdentity::load_or_create(&directory)?;
    if !directory.join("public.identity").exists() {
        identity.publish_identity(directory.join("public.identity"))?;
    }
    ensure!(
        IrohPeerIdentity::load(directory.join("public.identity"))? == identity.public_id(),
        "public identity differs from private key"
    );
    while !directory.join("source.profile").exists() {
        if shared.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        thread::park_timeout(Duration::from_millis(100));
    }
    let profile = IrohConnectionProfile::load(directory.join("source.profile"))?;
    let host = IrohHost::bind_with_identity_and_dns(profile.network(), &identity, platform::DNS)?;
    let owner = thread::current();
    let notifier = IrohNotifier::new(move || {
        owner.unpark();
        Ok(())
    });
    let mut pending = host.begin_connect_profile(
        &profile,
        IrohReceiverPreferences {
            codecs: vec![VideoCodec::Av1, VideoCodec::H264],
            stream_mode: SurfaceStreamMode::Composited,
        },
        notifier,
        Duration::from_secs(15),
    )?;
    shared.message("Connecting to approved source");
    let peer = loop {
        if shared.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(peer) = pending.poll()? {
            break peer;
        }
        thread::park_timeout(Duration::from_millis(50));
    };
    let connection = Connection(peer);
    shared.message(format!(
        "Live {:?}: {}x{} at {} Hz",
        connection.0.codec(),
        settings.width,
        settings.height,
        settings.fps
    ));
    let backend = Box::new(TimedBackend(platform::backend(connection.0.codec())?));
    let registration = destination_registration_with_backend(
        connection.0.clone(),
        ClientSourceId::new(0),
        ClientSourceId::new(1),
        Publisher,
        backend,
        None,
    );
    let mut runtime = ClientRuntime::default();
    runtime.register(registration.into_parts().runtime)?;
    let mut events = ClientPresentationInbox::default();
    let mut invalid_events = Vec::new();
    let mut invalid_effects = Vec::new();
    let mut selected = None;
    let mut last_active = true;
    while !shared.stopped.load(Ordering::Acquire) && connection.0.is_available() {
        runtime.drain_events(&mut events, &mut invalid_events);
        runtime.apply_pending_effects(&mut invalid_effects);
        runtime.apply_pending_presentations(&mut invalid_effects);
        ensure!(
            invalid_events.is_empty() && invalid_effects.is_empty(),
            "invalid receiver effects"
        );
        while let Some(event) = events.pop_front() {
            let surface = event.surface;
            match event.kind {
                ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(_)) => {
                    if selected.is_none() {
                        selected = Some(surface);
                        for kind in [
                            ClientSurfaceRequestKind::SetPreferredScale {
                                scale_120: Some(120),
                            },
                            ClientSurfaceRequestKind::Configure {
                                logical_size: Extent::new(settings.width, settings.height),
                                layout: ToplevelLayout::Tiled,
                                resizing: false,
                                fullscreen: false,
                            },
                        ] {
                            ensure!(
                                runtime.apply_request(ClientRequest::Surface(
                                    ClientSurfaceRequest { surface, kind }
                                )),
                                "sizing rejected"
                            );
                        }
                    }
                    ensure!(
                        runtime.apply_request(ClientRequest::Surface(ClientSurfaceRequest {
                            surface,
                            kind: ClientSurfaceRequestKind::SetPresentation {
                                rate: (selected == Some(surface)).then_some(settings.rate())
                            }
                        })),
                        "cadence rejected"
                    );
                }
                ClientSurfaceEventKind::Commit(commit) if selected == Some(surface) => {
                    let commit = commit.into_state();
                    let Some(root) = commit.root.filter(|_| commit.mapped) else {
                        let retired = shared
                            .frames
                            .lock()
                            .map_err(|_| anyhow::anyhow!("mailbox poisoned"))?
                            .drain();
                        drop(retired);
                        shared.clear.store(true, Ordering::Release);
                        continue;
                    };
                    for buffer in commit.buffers {
                        if buffer.layer == root.layer
                            && let SurfaceBufferChange::Replaced { buffer: lease, .. } =
                                buffer.change
                        {
                            let slot = lease
                                .access::<RefCell<Option<TimedImage>>>()
                                .context("native image lease")?;
                            if let Some(image) = slot.try_borrow_mut()?.take() {
                                let old = shared
                                    .frames
                                    .lock()
                                    .map_err(|_| anyhow::anyhow!("mailbox poisoned"))?
                                    .push(
                                        Frame {
                                            image: image.image,
                                            decoded_at: image.decoded_at,
                                            ready: Instant::now(),
                                        },
                                        Instant::now(),
                                    );
                                drop(old);
                                shared.request_redraw();
                            }
                        }
                    }
                }
                ClientSurfaceEventKind::Destroyed if selected == Some(surface) => {
                    shared.message("Application closed");
                    return Ok(());
                }
                _ => {}
            }
        }
        let active =
            shared.active.load(Ordering::Acquire) && !shared.paused.load(Ordering::Acquire);
        if active != last_active
            && let Some(surface) = selected
        {
            runtime.apply_request(ClientRequest::Surface(ClientSurfaceRequest {
                surface,
                kind: ClientSurfaceRequestKind::SetPresentation {
                    rate: active.then_some(settings.rate()),
                },
            }));
            last_active = active;
        }
        thread::park_timeout(
            runtime
                .next_deadline()
                .map_or(Duration::from_millis(50), |at| {
                    at.saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(50))
                }),
        );
    }
    shared.message("Disconnected");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Settings;
    #[test]
    fn probe_workload_bounds_reject_zero_and_excessive_requests() {
        for (width, height, fps) in [
            (0, 1080, 60),
            (1920, 0, 60),
            (4097, 1080, 60),
            (1920, 4097, 60),
            (1920, 1080, 0),
            (1920, 1080, 121),
        ] {
            assert!(
                Settings {
                    width,
                    height,
                    fps,
                    queue: super::QueuePolicy::Smoothing,
                    nonblocking_poll: true
                }
                .validate()
                .is_err()
            );
        }
        let settings = Settings {
            width: 1920,
            height: 1080,
            fps: 60,
            queue: super::QueuePolicy::Smoothing,
            nonblocking_poll: true,
        };
        assert!(settings.validate().is_ok());
        assert_eq!(settings.rate().millihertz(), 60_000);
    }
}
