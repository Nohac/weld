//! Iroh/client-runtime coordinator. Only owned native images and input geometry
//! cross to presentation; protocol leases remain on this thread.
use anyhow::{Context, Result, ensure};
use std::{
    cell::RefCell,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use weld_client::{
    ClientFocusRequest, ClientPointerRoute, ClientPresentationInbox, ClientRequest, ClientRuntime,
    ClientSourceId, ClientSurfaceEventKind, ClientSurfaceId, ClientSurfaceRequest,
    ClientSurfaceRequestKind, ClientSurfaceRole, InputPosition, PresentationMailbox,
    PresentationRate, RuntimeInputEvent, RuntimeInputEventKind, SurfaceBufferChange,
    SurfaceContentView, SurfaceInputGeometry, TouchEvent,
};
use weld_hoist_encoded::EncodedDestinationTransport;
use weld_hoist_encoded::android::{AndroidDecodeBackend, AndroidFramePublisher};
use weld_hoist_iroh::pairing::{ApplicationInfo, PairingInvitation, PairingProgress};
use weld_hoist_iroh::{
    IrohConnectionProfile, IrohDestinationPeer, IrohDeviceIdentity, IrohDnsPolicy, IrohHost,
    IrohNotifier, IrohPeerIdentity, destination_registration_with_backend,
};
use weld_media::VideoCodec;
use weld_media_android::AndroidImage;

use crate::geometry::WindowPreference;
use crate::startup::InitialPresentation;

pub(super) struct Frame {
    pub ready_at: Instant,
    pub epoch: u64,
    pub image: AndroidImage,
    pub view: SurfaceContentView,
    pub input: SurfaceInputGeometry,
}
#[derive(Default)]
pub(super) struct Shared {
    pub reports: super::reporting::Reports,
    pub show_report: AtomicBool,
    collect_report: AtomicBool,
    redraw: Option<Arc<dyn Fn() + Send + Sync>>,
    pub latest: Mutex<PresentationMailbox<Frame>>,
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
    pub fn request_redraw(&self) {
        if let Some(redraw) = &self.redraw {
            redraw();
        }
    }
    pub fn message(&self, message: impl Into<String>) {
        let message = message.into();
        tracing::info!("{message}");
        if let Ok(mut status) = self.status.lock() {
            *status = message;
        }
        self.request_redraw();
    }
    fn clear(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let retired = self.latest.lock().ok().map(|mut queue| queue.drain());
        drop(retired);
        if let Ok(mut displayed) = self.displayed.lock() {
            *displayed = None;
        }
        self.request_redraw();
    }
}
pub(super) struct Input {
    pub epoch: u64,
    pub route: ClientPointerRoute,
    pub event: TouchEvent,
    pub focus: bool,
    pub time: u32,
}
pub(super) struct Session {
    pub shared: Arc<Shared>,
    input: SyncSender<Input>,
    worker: Option<JoinHandle<()>>,
}
impl Session {
    pub fn show_report(&self, show: bool) {
        self.shared.show_report.store(show, Ordering::Release);
        self.shared.request_redraw();
    }
    pub fn collect_report(&self) {
        self.shared.reports.notice("Collection requested. Reconnect if offline. This shares this session's sanitized report with its host.");
        self.shared.collect_report.store(true, Ordering::Release);
        self.shared.request_redraw();
        self.wake();
    }
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
    /// Return true when Android should leave the app instead of returning a hoist.
    pub fn back(&self) -> bool {
        if self.shared.show_report.swap(false, Ordering::AcqRel) {
            self.shared.request_redraw();
            return false;
        }
        if self.shared.paired.load(Ordering::Acquire)
            && !self.shared.browser.load(Ordering::Acquire)
        {
            self.release();
            false
        } else {
            self.reset_input();
            true
        }
    }
    pub fn reconnect(&self) {
        self.shared.retry.store(true, Ordering::Release);
        self.wake();
    }
    pub fn start(directory: PathBuf, redraw: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        let shared = Arc::new(Shared {
            redraw: Some(Arc::new(redraw)),
            latest: Mutex::new(PresentationMailbox::smoothing(
                PresentationRate::HZ_60.interval(),
            )),
            ..Default::default()
        });
        shared.active.store(true, Ordering::Release);
        shared.reports.open(directory.clone());
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
        if let Err(error) = shared.reports.persist_finished() {
            tracing::warn!(%error, "could not save phone diagnostic report");
        }
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
                if let Some((owner, report)) = shared.reports.saved_report()
                    && owner == profile.peer().as_str()
                {
                    host.diagnostics()
                        .restore_receiver_report(profile.peer(), report)?;
                }
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
    if let Some(recorder) = connection.0.diagnostics() {
        shared
            .reports
            .attach(connection.0.identity().as_str().to_owned(), recorder);
    }
    shared.message(format!(
        "Connected ({:?}); choose an application",
        connection.0.codec()
    ));
    let owner = thread::current();
    let backend = AndroidDecodeBackend::new(connection.0.codec(), move || owner.unpark());
    let registration = destination_registration_with_backend(
        connection.0.clone(),
        ClientSourceId::new(0),
        ClientSourceId::new(1),
        AndroidFramePublisher,
        Box::new(backend),
        None,
    );
    let mut runtime = ClientRuntime::default();
    runtime.register(registration.into_parts().runtime)?;
    let mut selected = None;
    let mut requested = None;
    let mut configured = None;
    let mut initial = InitialPresentation::default();
    let mut presentation = ClientPresentationInbox::default();
    let mut invalid_events = Vec::new();
    let mut invalid_effects = Vec::new();
    let mut active = shared.active.load(Ordering::Acquire);
    while !shared.stopped.load(Ordering::Acquire)
        && connection.0.is_available()
        && !has_invitation(shared)
        && !shared.retry.load(Ordering::Acquire)
    {
        if let Some(device) = &device {
            if shared.collect_report.swap(false, Ordering::AcqRel) {
                let result = shared
                    .reports
                    .for_peer(connection.0.identity().as_str())
                    .and_then(|report| device.collect_diagnostics(report));
                if let Err(error) = result {
                    shared.reports.notice(error.to_string());
                }
                shared.request_redraw();
            }
            if let Some(reply) = device.take_diagnostics() {
                match reply {
                    Some(report) => if let Err(error) = shared.reports.merge(report) { shared.reports.notice(error.to_string()); },
                    None => shared.reports.notice("Host denied collection, report expired, or collection was too frequent. Grant session diagnostics with weldctl devices diagnostics."),
                }
                shared.request_redraw();
            }
            if let Some(error) = device.take_error() {
                shared.message(error);
                shared.clear();
                shared.browser.store(true, Ordering::Release);
                if let Ok(mut selection) = shared.selection.lock() {
                    *selection = None;
                }
                requested = None;
                selected = None;
                configured = None;
                initial = InitialPresentation::default();
                reset(&mut runtime);
            }
            if let Ok(mut catalogue) = shared.catalogue.lock() {
                let next = device.applications();
                if *catalogue != next {
                    *catalogue = next;
                    shared.request_redraw();
                }
            }
            if shared.release.swap(false, Ordering::AcqRel) {
                device.release()?;
                selected = None;
                configured = None;
                initial = InitialPresentation::default();
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
        let preference = *shared
            .preference
            .lock()
            .map_err(|_| anyhow::anyhow!("preference mailbox poisoned"))?;
        if let (Some(surface), Some(preference)) = (selected, preference) {
            configure(
                &mut runtime,
                surface,
                preference,
                &mut configured,
                &mut initial,
            )?;
        }
        runtime.drain_events(&mut presentation, &mut invalid_events);
        runtime.apply_pending_effects(&mut invalid_effects);
        runtime.apply_pending_presentations(&mut invalid_effects);
        ensure!(
            invalid_events.is_empty() && invalid_effects.is_empty(),
            "invalid receiver events/effects"
        );
        while let Some(event) = presentation.pop_front() {
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
                        initial = InitialPresentation::default();
                        if let Some(preference) = preference {
                            configure(&mut runtime, id, preference, &mut configured, &mut initial)?;
                        }
                        shared.message("Preparing application for the phone display");
                    }
                    rate(&mut runtime, id, selected == Some(id) && active)?;
                }
                ClientSurfaceEventKind::Commit(commit) if selected == Some(id) => {
                    let commit = commit.into_state();
                    let Some(root) = commit.root.filter(|_| commit.mapped) else {
                        shared.clear();
                        configured = None;
                        initial = InitialPresentation::default();
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
                                let frame = Frame {
                                    ready_at: Instant::now(),
                                    epoch: shared.epoch.load(Ordering::Acquire),
                                    image,
                                    view,
                                    input,
                                };
                                let first = initial.is_waiting();
                                if let Some(frame) = initial.offer(
                                    frame,
                                    [view.logical_width, view.logical_height],
                                    Instant::now(),
                                ) {
                                    if first {
                                        tracing::info!(
                                            width = view.logical_width,
                                            height = view.logical_height,
                                            "presenting initial phone frame"
                                        );
                                        shared.message(
                                            "Streaming; Back returns to the application list",
                                        );
                                    }
                                    publish(shared, frame)?;
                                }
                            }
                        }
                    }
                }
                ClientSurfaceEventKind::Destroyed if selected == Some(id) => {
                    selected = None;
                    configured = None;
                    initial = InitialPresentation::default();
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
        if let Some(frame) = initial.poll(Instant::now()) {
            tracing::info!("presenting application's chosen size after initial sizing deadline");
            shared.message("Streaming; application chose its own size");
            publish(shared, frame)?;
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
            // Touch has its own captured routes; it never changes pointer hover.
            if input.focus {
                runtime.apply_request(ClientRequest::Focus(ClientFocusRequest {
                    source: input.route.surface.source(),
                    surface: Some(input.route.surface),
                }));
            }
            runtime.dispatch_touch(Some(input.route), input.event, input.time);
        }
        let wait = runtime
            .next_deadline()
            .into_iter()
            .chain(initial.deadline())
            .min()
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

fn configure(
    runtime: &mut ClientRuntime,
    surface: ClientSurfaceId,
    preference: WindowPreference,
    configured: &mut Option<(ClientSurfaceId, WindowPreference)>,
    initial: &mut InitialPresentation<Frame>,
) -> Result<()> {
    if *configured == Some((surface, preference)) {
        return Ok(());
    }
    for kind in preference.requests() {
        ensure!(
            runtime.apply_request(ClientRequest::Surface(ClientSurfaceRequest {
                surface,
                kind
            })),
            "phone sizing request rejected"
        );
    }
    initial.configure(preference, Instant::now());
    *configured = Some((surface, preference));
    tracing::info!(
        ?surface,
        ?preference,
        "requested tiled phone size and scale"
    );
    Ok(())
}

fn publish(shared: &Shared, frame: Frame) -> Result<()> {
    let (replaced, retired, wake) = {
        let mut queue = shared
            .latest
            .lock()
            .map_err(|_| anyhow::anyhow!("frame mailbox poisoned"))?;
        let retired = if queue.newest().is_some_and(|previous| {
            previous.epoch != frame.epoch
                || previous.view != frame.view
                || previous.input != frame.input
        }) {
            Some(queue.drain())
        } else {
            None
        };
        let wake = queue.is_empty();
        let replaced = queue.push(frame, Instant::now());
        (replaced, retired, wake)
    };
    drop(replaced);
    drop(retired);
    if wake {
        shared.request_redraw();
    }
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
    use weld_client::Extent;
    use weld_client::{ClientId, InputTransform, SurfaceLayerId};

    fn input() -> Input {
        Input {
            epoch: 0,
            route: ClientPointerRoute {
                surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1),
                layer: SurfaceLayerId::new(1),
                transform: InputTransform::IDENTITY,
            },
            event: TouchEvent::Down {
                id: weld_client::TouchId(1),
                position: weld_client::InputPosition::default(),
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
    fn back_returns_the_active_hoist_before_leaving_the_browser() {
        let shared = Arc::new(Shared::default());
        shared.paired.store(true, Ordering::Release);
        let (sender, _receiver) = mpsc::sync_channel(1);
        let session = Session {
            shared: shared.clone(),
            input: sender,
            worker: None,
        };
        let epoch = shared.epoch.load(Ordering::Acquire);
        assert!(!session.back());
        assert!(shared.browser.load(Ordering::Acquire));
        assert!(shared.release.load(Ordering::Acquire));
        assert!(shared.reset.load(Ordering::Acquire));
        assert!(shared.epoch.load(Ordering::Acquire) > epoch);
        shared.release.store(false, Ordering::Release);
        assert!(session.back());
        assert!(!shared.release.load(Ordering::Acquire));
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
