//! Iroh/client-runtime coordinator. Only owned native images and input geometry
//! cross to presentation; protocol leases remain on this thread.
use anyhow::{Context, Result, ensure};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, ClientEventQueue,
    ClientFocusRequest, ClientPointerRoute, ClientRequest, ClientRuntime, ClientSourceId,
    ClientSurfaceEventKind, ClientSurfaceId, ClientSurfaceRequest, ClientSurfaceRequestKind,
    ClientSurfaceRole, Extent, InputEventKind, InputPosition, PresentationRate, RuntimeInputEvent,
    RuntimeInputEventKind, SurfaceBufferChange, SurfaceContentView, SurfaceInputGeometry,
};
use weld_hoist_encoded::{DecodedFramePublisher, EncodedDestinationTransport};
use weld_hoist_iroh::pairing::{ApplicationInfo, PairingInvitation, PairingProgress};
use weld_hoist_iroh::{
    IrohConnectionProfile, IrohDestinationPeer, IrohDeviceIdentity, IrohDnsPolicy, IrohHost,
    IrohNotifier, IrohPeerIdentity, destination_registration_with_backend,
};
use weld_media::VideoCodec;
use weld_media_android::AndroidImage;

use super::decode::Backend;
use crate::geometry::WindowPreference;

pub(super) struct Frame {
    pub epoch: u64,
    pub image: AndroidImage,
    pub view: SurfaceContentView,
    pub input: SurfaceInputGeometry,
}
#[derive(Default)]
pub(super) struct Shared {
    pub latest: Mutex<Option<Frame>>,
    pub displayed: Mutex<Option<(u64, SurfaceInputGeometry)>>,
    pub status: Mutex<String>,
    pub stopped: AtomicBool,
    pub active: AtomicBool,
    pub epoch: AtomicU64,
    reset: AtomicBool,
    preference: Mutex<Option<WindowPreference>>,
    pub catalogue: Mutex<Vec<ApplicationInfo>>,
    pub browser: AtomicBool,
    pub paired: AtomicBool,
    pub selection: Mutex<Option<ApplicationInfo>>,
    invitation: Mutex<Option<(PairingInvitation, String)>>,
    release: AtomicBool,
    retry: AtomicBool,
    mode: AtomicU8,
}
impl Shared {
    pub fn message(&self, message: impl Into<String>) {
        let message = message.into();
        tracing::info!("{message}");
        if let Ok(mut status) = self.status.lock() {
            *status = message;
        }
    }
    fn clear(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut frame) = self.latest.lock() {
            *frame = None;
        }
        if let Ok(mut displayed) = self.displayed.lock() {
            *displayed = None;
        }
    }
}
pub(super) struct Input {
    pub epoch: u64,
    pub route: ClientPointerRoute,
    pub position: InputPosition,
    pub event: InputEventKind,
    pub focus: bool,
    pub time: u32,
}
pub(super) struct Session {
    pub shared: Arc<Shared>,
    input: SyncSender<Input>,
    worker: Option<JoinHandle<()>>,
}
impl Session {
    pub fn set_development(&self, development: bool) {
        let mode = if development { 2 } else { 1 };
        let previous = self.shared.mode.swap(mode, Ordering::AcqRel);
        if previous != 0 && previous != mode {
            self.reconnect();
        }
        if previous == 0 {
            self.wake();
        }
    }
    pub fn pair(&self, link: &str, name: String) {
        match link.parse() {
            Ok(invitation) => {
                self.shared.browser.store(true, Ordering::Release);
                self.shared.clear();
                if let Ok(mut pending) = self.shared.invitation.lock() {
                    *pending = Some((invitation, name));
                }
                self.reset_input();
            }
            Err(_) => self.shared.message("Not a valid Weld pairing link"),
        }
    }
    pub fn hoist(&self, application: ApplicationInfo) {
        self.shared.clear();
        if let Ok(mut selection) = self.shared.selection.lock() {
            *selection = Some(application);
        }
        self.shared.browser.store(false, Ordering::Release);
        self.reset_input();
    }
    pub fn release(&self) {
        self.shared.message("Running applications");
        self.shared.clear();
        if let Ok(mut selection) = self.shared.selection.lock() {
            *selection = None;
        }
        self.shared.browser.store(true, Ordering::Release);
        self.shared.release.store(true, Ordering::Release);
        self.reset_input();
    }
    pub fn reconnect(&self) {
        self.shared.retry.store(true, Ordering::Release);
        self.wake();
    }
    pub fn start(directory: PathBuf) -> Result<Self> {
        let shared = Arc::new(Shared::default());
        shared.active.store(true, Ordering::Release);
        let context = shared.clone();
        let (input, events) = mpsc::sync_channel(128);
        let worker = thread::Builder::new()
            .name("mobile-receiver".into())
            .spawn(move || {
                if let Err(error) = run(directory, &context, events) {
                    context.message(format!("Receiver stopped: {error:#}"));
                    context.clear();
                }
            })?;
        Ok(Self {
            shared,
            input,
            worker: Some(worker),
        })
    }
    pub fn input(&self, input: Input) -> bool {
        let accepted = self.input.try_send(input).is_ok();
        if !accepted {
            self.shared.reset.store(true, Ordering::Release);
        }
        self.wake();
        accepted
    }
    pub fn set_active(&self, active: bool) {
        if self.shared.active.swap(active, Ordering::AcqRel) != active {
            self.reset_input();
        }
    }
    pub fn set_preference(&self, preference: WindowPreference) {
        if let Ok(mut pending) = self.shared.preference.lock() {
            if *pending == Some(preference) {
                return;
            }
            *pending = Some(preference);
        }
        self.wake();
    }
    pub fn reset_input(&self) {
        self.shared.reset.store(true, Ordering::Release);
        self.wake();
    }
    fn wake(&self) {
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        self.wake();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("mobile receiver panicked");
        }
    }
}

struct Publisher;
impl DecodedFramePublisher for Publisher {
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
struct Connection(IrohDestinationPeer);
impl Drop for Connection {
    fn drop(&mut self) {
        self.0.disconnect();
    }
}

fn run(directory: PathBuf, shared: &Shared, input: Receiver<Input>) -> Result<()> {
    let identity = IrohDeviceIdentity::load_or_create(&directory)?;
    let public = directory.join("public.identity");
    if !public.try_exists()? {
        identity.publish_identity(&public)?;
    }
    ensure!(
        IrohPeerIdentity::load(&public)? == identity.public_id(),
        "public identity differs from private key"
    );
    let paired_profile = directory.join("paired.profile");
    let development_profile = directory.join("source.profile");
    shared.browser.store(true, Ordering::Release);
    shared.message("Open a Weld pairing link or use Paste pairing link");
    let mut connect = true;
    while !shared.stopped.load(Ordering::Acquire) {
        let mode = shared.mode.load(Ordering::Acquire);
        if mode == 0 {
            thread::park_timeout(Duration::from_millis(100));
            continue;
        }
        let invitation = shared
            .invitation
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing inbox poisoned"))?
            .take();
        if let Some((invitation, name)) = invitation {
            let result = enroll(&identity, invitation, name, shared, &paired_profile);
            match result {
                Ok(()) => connect = true,
                Err(error) => shared.message(format!("Pairing failed: {error:#}")),
            }
        }
        connect |= shared.retry.swap(false, Ordering::AcqRel);
        let paired = mode == 1 && paired_profile.try_exists()?;
        let path = if paired {
            &paired_profile
        } else {
            &development_profile
        };
        if connect && (paired || mode == 2) && path.try_exists()? {
            connect = false;
            shared.paired.store(paired, Ordering::Release);
            shared.browser.store(paired, Ordering::Release);
            let result = IrohConnectionProfile::load(path).and_then(|profile| {
                let host = IrohHost::bind_with_identity_and_dns(
                    profile.network(),
                    &identity,
                    IrohDnsPolicy::Public,
                )?;
                stream(&host, &profile, shared, &input, paired)
            });
            shared.clear();
            if let Ok(mut catalogue) = shared.catalogue.lock() {
                catalogue.clear();
            }
            if let Ok(mut selected) = shared.selection.lock() {
                *selected = None;
            }
            shared.browser.store(true, Ordering::Release);
            if let Err(error) = result {
                shared.message(format!("Disconnected: {error:#}. Tap Reconnect to retry."));
            }
        }
        thread::park_timeout(Duration::from_millis(100));
    }
    Ok(())
}

fn has_invitation(shared: &Shared) -> bool {
    shared
        .invitation
        .lock()
        .is_ok_and(|pending| pending.is_some())
}

fn enroll(
    identity: &IrohDeviceIdentity,
    invitation: PairingInvitation,
    name: String,
    shared: &Shared,
    destination: &std::path::Path,
) -> Result<()> {
    let profile = invitation.profile()?;
    let host =
        IrohHost::bind_with_identity_and_dns(profile.network(), identity, IrohDnsPolicy::Public)?;
    let owner = thread::current();
    let notifier = IrohNotifier::new(move || {
        owner.unpark();
        Ok(())
    });
    let pending = host.begin_pairing(invitation, name, notifier)?;
    shared.message("Connecting for pairing");
    let mut shown = None;
    loop {
        ensure!(
            !shared.stopped.load(Ordering::Acquire) && !has_invitation(shared),
            "pairing cancelled"
        );
        match pending.progress() {
            PairingProgress::Connecting => {}
            PairingProgress::Verify { host, code } => {
                if shown.as_ref() != Some(&code) {
                    shared.message(format!("Pair with {host}: {code}\nConfirm this code on the desktop to allow browsing and hoisting."));
                    shown = Some(code);
                }
            }
            PairingProgress::Approved { .. } => {
                profile.save(destination)?;
                shared.message("Paired. Loading running applications");
                return Ok(());
            }
            PairingProgress::Failed(error) => anyhow::bail!("{error}"),
        }
        thread::park_timeout(Duration::from_millis(100));
    }
}

fn stream(
    host: &IrohHost,
    profile: &IrohConnectionProfile,
    shared: &Shared,
    input: &Receiver<Input>,
    paired: bool,
) -> Result<()> {
    let owner = thread::current();
    let notifier = IrohNotifier::new(move || {
        owner.unpark();
        Ok(())
    });
    shared.message("Connecting to approved Weld host");
    let mut device = None;
    let peer = if paired {
        let mut pending = host.begin_device_session(
            profile.clone(),
            vec![VideoCodec::Av1, VideoCodec::H264],
            notifier.clone(),
        )?;
        loop {
            ensure!(
                !shared.stopped.load(Ordering::Acquire) && !has_invitation(shared),
                "connection cancelled"
            );
            if let Some(session) = pending.poll()? {
                let peer = session.peer.clone();
                device = Some(session);
                break peer;
            }
            thread::park_timeout(Duration::from_millis(100));
        }
    } else {
        let mut pending = host.begin_connect_profile(
            profile,
            vec![VideoCodec::Av1, VideoCodec::H264],
            notifier,
            Duration::from_secs(10),
        )?;
        loop {
            if shared.stopped.load(Ordering::Acquire) || has_invitation(shared) {
                return Ok(());
            }
            if let Some(peer) = pending.poll()? {
                break peer;
            }
            thread::park_timeout(Duration::from_millis(100));
        }
    };
    let connection = Connection(peer);
    shared.message(format!(
        "Connected ({:?}); choose an application",
        connection.0.codec()
    ));
    let backend = Backend::new(connection.0.codec())?;
    let registration = destination_registration_with_backend(
        connection.0.clone(),
        ClientSourceId::new(0),
        ClientSourceId::new(1),
        Publisher,
        Box::new(backend),
        None,
    );
    let mut runtime = ClientRuntime::default();
    runtime.register(registration.into_parts().runtime)?;
    let mut selected = None;
    let mut requested = None;
    let mut configured = None;
    let mut events = ClientEventQueue::default();
    let mut invalid_events = Vec::new();
    let mut invalid_effects = Vec::new();
    let mut active = shared.active.load(Ordering::Acquire);
    while !shared.stopped.load(Ordering::Acquire)
        && connection.0.is_available()
        && !has_invitation(shared)
        && !shared.retry.load(Ordering::Acquire)
    {
        if let Some(device) = &device {
            if let Some(error) = device.take_error() {
                shared.message(error);
                shared.clear();
                shared.browser.store(true, Ordering::Release);
                if let Ok(mut selection) = shared.selection.lock() {
                    *selection = None;
                }
                requested = None;
                selected = None;
                reset(&mut runtime);
            }
            if let Ok(mut catalogue) = shared.catalogue.lock() {
                *catalogue = device.applications();
            }
            if shared.release.swap(false, Ordering::AcqRel) {
                device.release()?;
                selected = None;
                configured = None;
                requested = None;
                reset(&mut runtime);
            }
            let next = shared
                .selection
                .lock()
                .map_err(|_| anyhow::anyhow!("selection poisoned"))?
                .clone();
            if let Some(application) = next
                && requested != Some(application.window)
            {
                device.hoist(application.window)?;
                requested = Some(application.window);
            }
        }
        runtime.drain_events(&mut events, &mut invalid_events);
        runtime.apply_pending_effects(&mut invalid_effects);
        runtime.apply_pending_presentations(&mut invalid_effects);
        ensure!(
            invalid_events.is_empty() && invalid_effects.is_empty(),
            "invalid receiver events/effects"
        );
        while let Some(event) = events.pop_front() {
            let id = event.surface;
            match event.kind {
                ClientSurfaceEventKind::Role(role) => {
                    let wanted = !paired
                        || shared.selection.lock().is_ok_and(|selection| {
                            selection.as_ref().is_some_and(|application| {
                                application.surface.client().local() == id.client().local()
                                    && application.surface.local() == id.local()
                            })
                        });
                    if selected.is_none()
                        && wanted
                        && matches!(role, ClientSurfaceRole::Toplevel(_))
                    {
                        selected = Some(id);
                        configured = None;
                        shared.message("Streaming first window; touch to click or drag");
                    }
                    rate(&mut runtime, id, selected == Some(id) && active)?;
                }
                ClientSurfaceEventKind::Commit(commit) if selected == Some(id) => {
                    let commit = commit.into_state();
                    let Some(root) = commit.root.filter(|_| commit.mapped) else {
                        shared.clear();
                        reset(&mut runtime);
                        continue;
                    };
                    let view = commit
                        .window_geometry
                        .map_or(root.view, |geometry| geometry.view);
                    let origin =
                        commit
                            .window_geometry
                            .map_or(InputPosition::default(), |geometry| {
                                InputPosition::new(
                                    f64::from(geometry.origin.x),
                                    f64::from(geometry.origin.y),
                                )
                            });
                    for buffer in commit.buffers {
                        if buffer.layer == root.layer
                            && let SurfaceBufferChange::Replaced { buffer: lease, .. } =
                                buffer.change
                        {
                            let slot = lease
                                .access::<RefCell<Option<AndroidImage>>>()
                                .context("invalid Android image lease")?;
                            if let Some(image) = slot.try_borrow_mut()?.take() {
                                let input = SurfaceInputGeometry {
                                    surface: id,
                                    origin,
                                    logical_size: [
                                        f64::from(view.logical_width),
                                        f64::from(view.logical_height),
                                    ],
                                    inputs: commit
                                        .inputs
                                        .iter()
                                        .filter(|p| p.layer == root.layer)
                                        .cloned()
                                        .collect(),
                                };
                                *shared
                                    .latest
                                    .lock()
                                    .map_err(|_| anyhow::anyhow!("frame mailbox poisoned"))? =
                                    Some(Frame {
                                        epoch: shared.epoch.load(Ordering::Acquire),
                                        image,
                                        view,
                                        input,
                                    });
                            }
                        }
                    }
                }
                ClientSurfaceEventKind::Destroyed if selected == Some(id) => {
                    selected = None;
                    configured = None;
                    shared.clear();
                    reset(&mut runtime);
                    if paired {
                        shared.message("Running applications");
                        shared.browser.store(true, Ordering::Release);
                        if let Ok(mut selection) = shared.selection.lock() {
                            *selection = None;
                        }
                        requested = None;
                    }
                }
                _ => {}
            }
        }
        let next_active = shared.active.load(Ordering::Acquire);
        if next_active != active {
            active = next_active;
            if let Some(id) = selected {
                rate(&mut runtime, id, active)?;
            }
        }
        let preference = *shared
            .preference
            .lock()
            .map_err(|_| anyhow::anyhow!("preference mailbox poisoned"))?;
        if let (Some(surface), Some(preference)) = (selected, preference)
            && configured != Some((surface, preference))
        {
            for kind in [
                ClientSurfaceRequestKind::Configure {
                    logical_size: preference.size,
                    layout: Default::default(),
                    resizing: false,
                    fullscreen: false,
                },
                ClientSurfaceRequestKind::SetPreferredScale {
                    scale_120: Some(preference.scale_120),
                },
            ] {
                ensure!(
                    runtime.apply_request(ClientRequest::Surface(ClientSurfaceRequest {
                        surface,
                        kind
                    })),
                    "phone sizing request rejected"
                );
            }
            tracing::info!(
                ?surface,
                ?preference,
                "requested phone window size and scale"
            );
            configured = Some((surface, preference));
        }
        if shared.reset.swap(false, Ordering::AcqRel) {
            for _ in input.try_iter() {}
            reset(&mut runtime);
        }
        for input in input.try_iter() {
            if !active
                || selected != Some(input.route.surface)
                || input.epoch != shared.epoch.load(Ordering::Acquire)
            {
                continue;
            }
            runtime.set_pointer_route(Some(input.route));
            if input.focus {
                runtime.apply_request(ClientRequest::Focus(ClientFocusRequest {
                    source: input.route.surface.source(),
                    surface: Some(input.route.surface),
                }));
            }
            if !matches!(input.event, InputEventKind::PointerMotion { .. }) {
                runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
                    RuntimeInputEventKind::Input(InputEventKind::PointerMotion {
                        position: input.position,
                        relative: None,
                    }),
                    input.time,
                ));
            }
            let leave = matches!(
                input.event,
                InputEventKind::PointerButton {
                    state: weld_client::ButtonState::Released,
                    ..
                }
            );
            runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
                RuntimeInputEventKind::Input(input.event),
                input.time,
            ));
            if leave {
                runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
                    RuntimeInputEventKind::Input(InputEventKind::PointerLeft {
                        position: input.position,
                    }),
                    input.time,
                ));
                runtime.set_pointer_route(None);
            }
        }
        let wait = runtime
            .next_deadline()
            .map_or(Duration::from_millis(100), |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100))
            });
        thread::park_timeout(wait);
    }
    reset(&mut runtime);
    drop(connection);
    drop(runtime);
    shared.clear();
    shared.message("Disconnected; reopen to connect again");
    Ok(())
}
fn reset(runtime: &mut ClientRuntime) {
    runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
        RuntimeInputEventKind::HostFocusLost,
        0,
    ));
    runtime.apply_request(ClientRequest::ClearFocus);
}
fn rate(runtime: &mut ClientRuntime, surface: ClientSurfaceId, active: bool) -> Result<()> {
    ensure!(
        runtime.apply_request(ClientRequest::Surface(ClientSurfaceRequest {
            surface,
            kind: ClientSurfaceRequestKind::SetPresentation {
                rate: active.then_some(PresentationRate::HZ_60)
            },
        })),
        "presentation rate request rejected"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_client::{ClientId, InputTransform, SurfaceLayerId};

    fn input() -> Input {
        Input {
            epoch: 0,
            route: ClientPointerRoute {
                surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1),
                layer: SurfaceLayerId::new(1),
                transform: InputTransform::IDENTITY,
            },
            position: InputPosition::default(),
            event: InputEventKind::PointerMotion {
                position: InputPosition::default(),
                relative: None,
            },
            focus: false,
            time: 1,
        }
    }

    #[test]
    fn full_input_queue_requests_an_out_of_band_release() {
        let shared = Arc::new(Shared::default());
        let (sender, _receiver) = mpsc::sync_channel(1);
        let session = Session {
            shared: shared.clone(),
            input: sender,
            worker: None,
        };
        assert!(session.input(input()));
        assert!(!session.input(input()));
        assert!(shared.reset.load(Ordering::Acquire));
    }

    #[test]
    fn sizing_mailbox_keeps_only_latest_viewport() {
        let shared = Arc::new(Shared::default());
        let (sender, _receiver) = mpsc::sync_channel(1);
        let session = Session {
            shared: shared.clone(),
            input: sender,
            worker: None,
        };
        let portrait = WindowPreference {
            size: Extent::new(448, 880),
            scale_120: 360,
        };
        let landscape = WindowPreference {
            size: Extent::new(944, 384),
            scale_120: 360,
        };
        session.set_preference(portrait);
        session.set_preference(landscape);
        session.set_preference(landscape);
        assert_eq!(
            *shared.preference.lock().expect("preference"),
            Some(landscape)
        );
        assert!(!shared.reset.load(Ordering::Acquire));
    }

    #[test]
    fn hiding_requests_release_and_unmap_invalidates_displayed_target() {
        let shared = Arc::new(Shared::default());
        shared.active.store(true, Ordering::Release);
        let (sender, _receiver) = mpsc::sync_channel(1);
        let session = Session {
            shared: shared.clone(),
            input: sender,
            worker: None,
        };
        let geometry = SurfaceInputGeometry {
            surface: input().route.surface,
            origin: InputPosition::default(),
            logical_size: [100.0, 100.0],
            inputs: vec![],
        };
        *shared.displayed.lock().expect("displayed") = Some((0, geometry));
        session.reset_input();
        assert!(shared.active.load(Ordering::Acquire));
        assert!(shared.reset.swap(false, Ordering::AcqRel));
        session.set_active(false);
        assert!(!shared.active.load(Ordering::Acquire));
        assert!(shared.reset.load(Ordering::Acquire));
        shared.clear();
        assert!(shared.displayed.lock().expect("displayed").is_none());
        assert_eq!(shared.epoch.load(Ordering::Acquire), 1);
    }
}
