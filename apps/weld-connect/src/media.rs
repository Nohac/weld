//! Stream admission and frame publication on the session worker.
use anyhow::{Context, Result, ensure};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, ClientFocusRequest,
    ClientPointerRoute, ClientPresentationInbox, ClientRequest, ClientRuntime, ClientSourceId,
    ClientSurfaceEventKind, ClientSurfaceId, ClientSurfaceRequest, ClientSurfaceRequestKind,
    ClientSurfaceRole, Extent, InitialPresentation, InputPosition, PresentationMailbox,
    PresentationRate, RuntimeInputEvent, RuntimeInputEventKind, SurfaceBufferChange,
    SurfaceContentView, SurfaceInputGeometry, TouchEvent, WindowPreference,
};
use weld_hoist_encoded::DecodedFramePublisher;
use weld_hoist_iroh::{
    IrohDestinationPeer, destination_registration_with_backend, pairing::ApplicationInfo,
};
use weld_video_gles as video;

pub struct Frame {
    pub ready_at: Instant,
    pub image: video::Image,
    pub view: SurfaceContentView,
    pub input: SurfaceInputGeometry,
    pub epoch: u64,
}
pub struct Shared {
    pub frames: Mutex<PresentationMailbox<Frame>>,
    pub epoch: AtomicU64,
    pub active: AtomicBool,
    pub reset: AtomicBool,
    pub preference: Mutex<Option<WindowPreference>>,
    redraw: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}
impl Default for Shared {
    fn default() -> Self {
        Self {
            frames: Mutex::new(PresentationMailbox::smoothing(
                PresentationRate::HZ_60.interval(),
            )),
            epoch: AtomicU64::new(0),
            active: AtomicBool::new(false),
            reset: AtomicBool::new(false),
            preference: Mutex::new(None),
            redraw: Mutex::new(None),
        }
    }
}
impl Shared {
    pub fn attach(&self, redraw: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut slot) = self.redraw.lock() {
            *slot = Some(redraw);
        }
    }
    pub fn redraw(&self) {
        let redraw = self.redraw.lock().ok().and_then(|slot| slot.clone());
        if let Some(redraw) = redraw {
            redraw();
        }
    }
    pub fn clear(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let retired = self.frames.lock().ok().map(|mut frames| frames.drain());
        drop(retired);
        self.reset.store(true, Ordering::Release);
        self.redraw();
    }
}
pub struct Input {
    pub epoch: u64,
    pub route: Option<ClientPointerRoute>,
    pub event: TouchEvent,
    pub time: u32,
}
struct Publisher;
impl DecodedFramePublisher for Publisher {
    type Buffer = video::Image;
    type ClientImporter = ();
    fn client_importer(&self) {}
    fn publish(
        &mut self,
        image: Self::Buffer,
        id: ClientBufferId,
        use_id: ClientBufferUseId,
    ) -> Result<ClientBufferLease> {
        let [width, height] = video::extent(&image);
        Ok(ClientBufferLease::new(
            id,
            use_id,
            ClientBufferMetadata::new(Extent::new(width, height), true),
            Rc::new(RefCell::new(Some(image))),
            |_| {},
        )?)
    }
}
pub struct Receiver {
    runtime: ClientRuntime,
    shared: Arc<Shared>,
    pub requested: Option<ApplicationInfo>,
    selected: Option<ClientSurfaceId>,
    configured: Option<WindowPreference>,
    initial: InitialPresentation<Frame>,
    events: ClientPresentationInbox,
    active: bool,
}
impl Receiver {
    pub fn new(peer: IrohDestinationPeer, shared: Arc<Shared>) -> Result<Self> {
        let backend = video::backend(peer.codec())?;
        let registration = destination_registration_with_backend(
            peer,
            ClientSourceId::new(0),
            ClientSourceId::new(1),
            Publisher,
            backend,
            None,
        );
        let mut runtime = ClientRuntime::default();
        runtime.register(registration.into_parts().runtime)?;
        Ok(Self {
            runtime,
            shared,
            requested: None,
            selected: None,
            configured: None,
            initial: InitialPresentation::default(),
            events: ClientPresentationInbox::default(),
            active: false,
        })
    }
    pub fn select(&mut self, application: Option<ApplicationInfo>) {
        self.reset_input();
        self.shared.clear();
        self.requested = application;
        self.selected = None;
        self.configured = None;
        self.initial = InitialPresentation::default();
    }
    fn reset_input(&mut self) {
        self.runtime
            .dispatch_unconsumed_input(RuntimeInputEvent::new(
                RuntimeInputEventKind::HostFocusLost,
                0,
            ));
        self.runtime.apply_request(ClientRequest::ClearFocus);
    }
    pub fn input(&mut self, input: Input) {
        if !self.active || input.epoch != self.shared.epoch.load(Ordering::Acquire) {
            return;
        }
        if let Some(route) = input.route {
            if self.selected != Some(route.surface) {
                return;
            }
            if matches!(input.event, TouchEvent::Down { .. }) {
                self.runtime
                    .apply_request(ClientRequest::Focus(ClientFocusRequest {
                        source: route.surface.source(),
                        surface: Some(route.surface),
                    }));
            }
        }
        self.runtime
            .dispatch_touch(input.route, input.event, input.time);
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.runtime
            .next_deadline()
            .into_iter()
            .chain(self.initial.deadline())
            .min()
    }
    pub fn poll(&mut self) -> Result<()> {
        if self.shared.reset.swap(false, Ordering::AcqRel) {
            self.reset_input();
        }
        let active = self.shared.active.load(Ordering::Acquire);
        if self.active != active {
            self.active = active;
            self.reset_input();
            if let Some(surface) = self.selected {
                self.rate(surface, active)?;
            }
        }
        let preference = *self
            .shared
            .preference
            .lock()
            .map_err(|_| anyhow::anyhow!("viewport poisoned"))?;
        if let (Some(surface), Some(preference)) = (self.selected, preference) {
            self.configure(surface, preference)?;
        }
        let mut invalid_events = Vec::new();
        let mut invalid_effects = Vec::new();
        self.runtime
            .drain_events(&mut self.events, &mut invalid_events);
        self.runtime.apply_pending_effects(&mut invalid_effects);
        self.runtime
            .apply_pending_presentations(&mut invalid_effects);
        ensure!(
            invalid_events.is_empty() && invalid_effects.is_empty(),
            "invalid receiver effects"
        );
        while let Some(event) = self.events.pop_front() {
            let surface = event.surface;
            match event.kind {
                ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(_)) => {
                    if self.selected.is_none()
                        && self.requested.as_ref().is_some_and(|app| {
                            app.surface.client().local() == surface.client().local()
                                && app.surface.local() == surface.local()
                        })
                    {
                        self.selected = Some(surface);
                        if let Some(preference) = preference {
                            self.configure(surface, preference)?;
                        }
                    }
                    self.rate(surface, self.selected == Some(surface) && active)?;
                }
                ClientSurfaceEventKind::Commit(commit) if self.selected == Some(surface) => {
                    let commit = commit.into_state();
                    let Some(root) = commit.root.filter(|_| commit.mapped) else {
                        self.shared.clear();
                        self.configured = None;
                        self.initial = InitialPresentation::default();
                        self.reset_input();
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
                                .access::<RefCell<Option<video::Image>>>()
                                .context("native image lease")?;
                            if let Some(image) = slot.try_borrow_mut()?.take() {
                                let frame = Frame {
                                    ready_at: Instant::now(),
                                    image,
                                    view,
                                    epoch: self.shared.epoch.load(Ordering::Acquire),
                                    input: SurfaceInputGeometry {
                                        surface,
                                        origin,
                                        logical_size: [
                                            f64::from(view.logical_width),
                                            f64::from(view.logical_height),
                                        ],
                                        inputs: commit
                                            .inputs
                                            .iter()
                                            .filter(|input| input.layer == root.layer)
                                            .cloned()
                                            .collect(),
                                    },
                                };
                                if let Some(frame) = self.initial.offer(
                                    frame,
                                    [view.logical_width, view.logical_height],
                                    Instant::now(),
                                ) {
                                    self.publish(frame)?;
                                }
                            }
                        }
                    }
                }
                ClientSurfaceEventKind::Destroyed if self.selected == Some(surface) => {
                    self.select(None)
                }
                _ => {}
            }
        }
        if let Some(frame) = self.initial.poll(Instant::now()) {
            self.publish(frame)?;
        }
        Ok(())
    }
    fn configure(&mut self, surface: ClientSurfaceId, preference: WindowPreference) -> Result<()> {
        if self.configured == Some(preference) {
            return Ok(());
        }
        for kind in preference.requests() {
            ensure!(
                self.runtime
                    .apply_request(ClientRequest::Surface(ClientSurfaceRequest {
                        surface,
                        kind
                    })),
                "sizing rejected"
            );
        }
        self.initial.configure(preference, Instant::now());
        self.configured = Some(preference);
        self.shared.reset.store(true, Ordering::Release);
        Ok(())
    }
    fn rate(&mut self, surface: ClientSurfaceId, active: bool) -> Result<()> {
        ensure!(
            self.runtime
                .apply_request(ClientRequest::Surface(ClientSurfaceRequest {
                    surface,
                    kind: ClientSurfaceRequestKind::SetPresentation {
                        rate: active.then_some(PresentationRate::HZ_60)
                    }
                })),
            "cadence rejected"
        );
        Ok(())
    }
    fn publish(&self, frame: Frame) -> Result<()> {
        let mut frames = self
            .shared
            .frames
            .lock()
            .map_err(|_| anyhow::anyhow!("frame mailbox poisoned"))?;
        let retired = if frames.newest().is_some_and(|last| {
            last.epoch != frame.epoch || last.view != frame.view || last.input != frame.input
        }) {
            Some(frames.drain())
        } else {
            None
        };
        let previous = frames.push(frame, Instant::now());
        drop(frames);
        drop(previous);
        drop(retired);
        self.shared.redraw();
        Ok(())
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.reset_input();
        self.shared.clear();
    }
}
