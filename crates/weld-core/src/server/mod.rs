//! Smithay Wayland-server boundary shared by host backends.

mod adapter;
mod cursor;
mod dmabuf;
mod output;
mod popup;
mod presentation;
mod resize;
mod seat;
mod shm;
mod surface_tree;
mod toplevel;

pub use adapter::WaylandClientImporter;
pub(crate) use adapter::{
    WaylandClientBridge, WaylandClientWork, registration as client_registration,
};
pub(crate) use output::{OutputDescriptor, OutputMetrics, ServerOutputDefinition};
pub use surface_tree::{
    PendingSurfaceBufferContent, PendingSurfaceBufferUpdate, PendingSurfaceTreeSnapshot,
};

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsString,
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result, bail};
use smithay::{
    backend::allocator::dmabuf::DmabufSource,
    desktop::{PopupGrab, PopupManager},
    input::{Seat, SeatState, pointer::CursorImageStatus},
    output::Output,
    reexports::{
        calloop::{
            Interest, LoopHandle, Mode, PostAction,
            channel::{Channel, Event as ChannelEvent},
            generic::Generic,
        },
        wayland_server::{
            Client, Display, DisplayHandle,
            backend::{ClientData, ClientId as WaylandClientId, DisconnectReason},
        },
    },
    utils::Transform,
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        cursor_shape::CursorShapeManagerState,
        drm_syncobj::DrmSyncPointSource,
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        pointer_gestures::PointerGesturesState,
        selection::data_device::DataDeviceState,
        shell::xdg::{XdgShellState, decoration::XdgDecorationState},
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
    },
};
use tracing::{debug, warn};
use weld_client::{
    ClientBufferUseId, ClientId, ClientRequest, ClientSurfaceRequestKind, ClientSurfaceRole,
};

use crate::{
    OutputId,
    dmabuf::{DmabufCapabilities, DmabufEvent, DmabufReleaseId, DmabufSourceCache},
    input::{InputPosition, KeyboardRepeatMode, KeyboardRepeatTracker, LegacyKeyRepeat},
    surface::{SurfaceId, WindowInteractionRequestKind},
};
use cursor::CursorSurfaceStore;
use dmabuf::{DmabufProtocol, DmabufReleaseStore};
use output::install_output_metrics;
use popup::PopupStore;
use resize::{PendingResize, PendingResizeRequests};
use seat::OrdinaryImplicitGrab;
use toplevel::ToplevelStore;

// Keep this stable name in sync with scripts/run-app.
const WELD_SOCKET_NAME: &str = "weld-0";

/// Smithay state kept outside Bevy's ECS world.
pub struct ServerState {
    pub display_handle: DisplayHandle,
    pub socket_name: OsString,
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    _xdg_decoration_state: XdgDecorationState,
    _cursor_shape_manager_state: CursorShapeManagerState,
    _pointer_gestures_state: PointerGesturesState,
    shm_state: ShmState,
    dmabuf_protocol: DmabufProtocol,
    dmabuf_releases: DmabufReleaseStore,
    completed_dmabuf_uses: Vec<ClientBufferUseId>,
    dmabuf_sources: DmabufSourceCache,
    _viewporter_state: ViewporterState,
    _fractional_scale_manager_state: FractionalScaleManagerState,
    _output_manager_state: OutputManagerState,
    seat_state: SeatState<Self>,
    data_device_state: DataDeviceState,
    seat: Seat<Self>,
    outputs: HashMap<OutputId, ServerOutput>,
    primary_output: OutputId,
    initial_toplevel_size: Option<crate::surface::Extent>,
    toplevels: ToplevelStore,
    popups: PopupStore,
    popup_manager: PopupManager,
    popup_grab: Option<PopupGrab<Self>>,
    focused_toplevel: Option<SurfaceId>,
    keyboard_repeat_mode: KeyboardRepeatMode,
    legacy_key_repeat: LegacyKeyRepeat,
    keyboard_repeats: KeyboardRepeatTracker,
    keyboard_diagnostic_dirty: bool,
    pending_focus: Option<Option<SurfaceId>>,
    pending_resizes: PendingResizeRequests,
    pending_surface_events: WaylandClientBridge,
    presentation_requested: bool,
    next_presentation_id: u64,
    staged_frame_callbacks: VecDeque<(u64, Vec<presentation::SurfaceCallbacks>)>,
    presentation_claims: presentation::PresentationClaims,
    independent_callbacks: HashMap<SurfaceId, Vec<presentation::SurfaceCallbacks>>,
    next_surface_id: Option<u64>,
    next_client_id: Option<u64>,
    started_at: Instant,
    pointer_position: InputPosition,
    ordinary_implicit_grab: Option<OrdinaryImplicitGrab>,
    // This mirrors delivered presses only so host focus loss can synthesize
    // matching releases; ECS pointer routing remains the policy authority.
    pressed_pointer_buttons: HashSet<u32>,
    cursor_status: CursorImageStatus,
    shell_cursor: crate::cursor::CursorAppearance,
    shell_owns_cursor: bool,
    cursor_surfaces: CursorSurfaceStore,
    cursor_feedback_dirty: bool,
    presented_cursor: Option<crate::cursor::CursorImage>,
    shell_cursor_override: bool,
    dmabuf_blocker_installer: Option<Box<dyn Fn(DmabufSource, Client) -> bool>>,
    syncobj_blocker_installer: Option<Box<dyn Fn(DrmSyncPointSource, Client) -> bool>>,
}

pub(crate) struct ServerOptions<'a> {
    pub(crate) started_at: Instant,
    pub(crate) seat_name: &'a str,
    pub(crate) outputs: Vec<ServerOutputDefinition>,
    pub(crate) dmabuf_capabilities: Option<&'a DmabufCapabilities>,
    pub(crate) dmabuf_sources: DmabufSourceCache,
    pub(crate) socket_name: Option<&'a str>,
    pub(crate) keyboard_repeat_mode: KeyboardRepeatMode,
    pub(crate) initial_toplevel_size: Option<crate::surface::Extent>,
}

struct ServerOutput {
    native: Output,
    metrics: OutputMetrics,
    logical_position: (i32, i32),
}

impl ServerState {
    pub(crate) fn new<LoopData: 'static>(
        loop_handle: &LoopHandle<'static, LoopData>,
        display: Display<Self>,
        dmabuf_release_source: Channel<DmabufEvent>,
        client_bridge: WaylandClientBridge,
        server: fn(&mut LoopData) -> &mut Self,
        options: ServerOptions<'_>,
    ) -> Result<Self> {
        let ServerOptions {
            started_at,
            seat_name,
            outputs,
            dmabuf_capabilities,
            dmabuf_sources,
            socket_name: requested_socket_name,
            keyboard_repeat_mode,
            initial_toplevel_size,
        } = options;
        let display_handle = display.handle();
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let cursor_shape_manager_state = CursorShapeManagerState::new::<Self>(&display_handle);
        let pointer_gestures_state = PointerGesturesState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, []);
        let dmabuf_protocol = DmabufProtocol::new(&display_handle, dmabuf_capabilities)?;
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);
        let fractional_scale_manager_state =
            FractionalScaleManagerState::new::<Self>(&display_handle);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, seat_name);
        seat.add_keyboard(Default::default(), 200, 25)
            .context("failed to initialize the compositor keyboard keymap")?;
        seat.add_pointer();

        let primary_output = outputs
            .iter()
            .find(|output| output.primary)
            .map(|output| output.id)
            .context("server output layout has no primary output")?;
        if outputs.iter().filter(|output| output.primary).count() != 1 {
            bail!("server output layout must contain exactly one primary output");
        }
        let mut installed_outputs = HashMap::with_capacity(outputs.len());
        for definition in outputs {
            let output = Output::new(
                definition.descriptor.name,
                definition.descriptor.physical_properties,
            );
            let output_mode = definition.metrics.mode();
            output.create_global::<Self>(&display_handle);
            output.change_current_state(
                Some(output_mode),
                Some(Transform::Normal),
                Some(definition.metrics.scale()),
                Some(definition.logical_position.into()),
            );
            output.set_preferred(output_mode);
            if installed_outputs
                .insert(
                    definition.id,
                    ServerOutput {
                        native: output,
                        metrics: definition.metrics,
                        logical_position: definition.logical_position,
                    },
                )
                .is_some()
            {
                bail!("server output layout contains duplicate output IDs");
            }
        }

        let socket_name = requested_socket_name.unwrap_or(WELD_SOCKET_NAME);
        let listening_socket = ListeningSocketSource::with_name(socket_name)
            .with_context(|| {
                format!(
                    "failed to bind Weld Wayland socket {socket_name:?}; another compositor may already be using it"
                )
            })?;
        let socket_name = listening_socket.socket_name().to_os_string();
        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                let _accept_span = tracing::trace_span!(
                    target: crate::PROFILE_TARGET,
                    "host_accept_wayland_client"
                )
                .entered();
                let state = server(state);
                let Some(client_id) = state.allocate_client_id() else {
                    warn!("rejected a Wayland client because ClientId space is exhausted");
                    return;
                };
                match state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::new(client_id)))
                {
                    Ok(_) => tracing::trace!(
                        target: crate::PROFILE_TARGET,
                        "accepted Wayland client"
                    ),
                    Err(error) => warn!(%error, "rejected a Wayland client"),
                }
            })
            .context("failed to register the Wayland listening socket")?;

        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                move |_, display, state| {
                    let state = server(state);
                    // SAFETY: calloop owns this source for the complete event-loop lifetime, so
                    // the contained Display is not moved or accessed concurrently.
                    let result = unsafe { display.get_mut() }.dispatch_clients(state);
                    if let Err(error) = result {
                        warn!(%error, "Wayland client dispatch failed");
                    }
                    Ok(PostAction::Continue)
                },
            )
            .context("failed to register the Wayland display")?;

        loop_handle
            .insert_source(dmabuf_release_source, move |event, _, state| {
                if let ChannelEvent::Msg(event) = event {
                    match event {
                        DmabufEvent::GpuUseCompleted(use_id) => {
                            server(state).completed_dmabuf_uses.push(use_id);
                        }
                        DmabufEvent::LeaseCompleted(release) => {
                            server(state).complete_dmabuf_release(release);
                        }
                    }
                }
            })
            .map_err(|_| anyhow::anyhow!("failed to register DMA-BUF completion results"))?;

        let dmabuf_blocker_installer = dmabuf_capabilities.map(|_| {
            let blocker_handle = loop_handle.clone();
            Box::new(move |source: DmabufSource, client: Client| {
                blocker_handle
                    .insert_source(source, move |_, _, loop_data| {
                        let state = server(loop_data);
                        let display_handle = state.display_handle.clone();
                        if let Some(client_state) = client.get_data::<ClientState>() {
                            client_state
                                .compositor_state
                                .blocker_cleared(state, &display_handle);
                        }
                        Ok(())
                    })
                    .is_ok()
            }) as Box<dyn Fn(DmabufSource, Client) -> bool>
        });
        let syncobj_blocker_installer = dmabuf_protocol.explicit_sync_enabled().then(|| {
            let blocker_handle = loop_handle.clone();
            Box::new(move |source: DrmSyncPointSource, client: Client| {
                blocker_handle
                    .insert_source(source, move |_, _, loop_data| {
                        let state = server(loop_data);
                        let display_handle = state.display_handle.clone();
                        if let Some(client_state) = client.get_data::<ClientState>() {
                            client_state
                                .compositor_state
                                .blocker_cleared(state, &display_handle);
                        }
                        Ok(())
                    })
                    .is_ok()
            }) as Box<dyn Fn(DrmSyncPointSource, Client) -> bool>
        });

        let mut state = Self {
            display_handle,
            socket_name,
            compositor_state,
            xdg_shell_state,
            _xdg_decoration_state: xdg_decoration_state,
            _cursor_shape_manager_state: cursor_shape_manager_state,
            _pointer_gestures_state: pointer_gestures_state,
            shm_state,
            dmabuf_protocol,
            dmabuf_releases: DmabufReleaseStore::default(),
            completed_dmabuf_uses: Vec::new(),
            dmabuf_sources,
            _viewporter_state: viewporter_state,
            _fractional_scale_manager_state: fractional_scale_manager_state,
            _output_manager_state: output_manager_state,
            seat_state,
            data_device_state,
            seat,
            outputs: installed_outputs,
            primary_output,
            initial_toplevel_size,
            toplevels: ToplevelStore::default(),
            popups: PopupStore::default(),
            popup_manager: PopupManager::default(),
            popup_grab: None,
            focused_toplevel: None,
            keyboard_repeat_mode,
            legacy_key_repeat: LegacyKeyRepeat::default(),
            keyboard_repeats: KeyboardRepeatTracker::default(),
            keyboard_diagnostic_dirty: true,
            pending_focus: None,
            pending_resizes: PendingResizeRequests::default(),
            pending_surface_events: client_bridge,
            presentation_requested: false,
            next_presentation_id: 1,
            staged_frame_callbacks: VecDeque::new(),
            presentation_claims: presentation::PresentationClaims::default(),
            independent_callbacks: HashMap::new(),
            next_surface_id: Some(1),
            next_client_id: Some(1),
            started_at,
            pointer_position: InputPosition::default(),
            ordinary_implicit_grab: None,
            pressed_pointer_buttons: HashSet::new(),
            cursor_status: CursorImageStatus::default_named(),
            shell_cursor: crate::cursor::CursorAppearance::default(),
            shell_owns_cursor: true,
            cursor_surfaces: CursorSurfaceStore::default(),
            cursor_feedback_dirty: true,
            presented_cursor: None,
            shell_cursor_override: false,
            dmabuf_blocker_installer,
            syncobj_blocker_installer,
        };
        state.configure_keyboard_repeat();
        Ok(state)
    }

    pub(crate) fn update_output_metrics(
        &mut self,
        id: OutputId,
        metrics: OutputMetrics,
        logical_position: (i32, i32),
    ) {
        let Some(output) = self.outputs.get_mut(&id) else {
            return;
        };
        let metrics_changed = output.metrics != metrics;
        let position_changed = output.logical_position != logical_position;
        if !metrics_changed && !position_changed {
            return;
        }
        if metrics_changed {
            install_output_metrics(&output.native, output.metrics, metrics);
            output.metrics = metrics;
        }
        if position_changed {
            output
                .native
                .change_current_state(None, None, None, Some(logical_position.into()));
            output.logical_position = logical_position;
        }
        if metrics_changed {
            self.send_all_surface_scales();
        }
    }

    pub(crate) fn native_output(&self, id: OutputId) -> Option<Output> {
        self.outputs.get(&id).map(|output| output.native.clone())
    }

    fn primary_output(&self) -> &Output {
        &self.outputs[&self.primary_output].native
    }

    fn enter_primary_output(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        self.primary_output().enter(surface);
        output::send_preferred_surface_scale(self.primary_output(), surface);
    }

    fn leave_all_outputs(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        for output in self.outputs.values() {
            output.native.leave(surface);
        }
    }

    pub(crate) fn complete_dmabuf_release(&mut self, release: DmabufReleaseId) {
        self.dmabuf_releases.complete(release);
    }

    pub(crate) fn take_completed_dmabuf_uses(&mut self) -> Vec<ClientBufferUseId> {
        std::mem::take(&mut self.completed_dmabuf_uses)
    }

    pub const fn presentation_requested(&self) -> bool {
        self.presentation_requested
    }

    pub fn flush_clients(&mut self) {
        self.popup_manager.cleanup();
        if self.popup_grab.as_ref().is_some_and(PopupGrab::has_ended) {
            self.popup_grab = None;
        }
        if let Err(error) = self.display_handle.flush_clients() {
            warn!(%error, "failed to flush Wayland clients");
        }
    }

    pub(crate) fn apply_pending_client_work(&mut self) {
        while let Some(work) = self.pending_surface_events.pop_work() {
            match work {
                WaylandClientWork::Request(request) => self.apply_client_request(request),
                WaylandClientWork::Input(event) => self.apply_client_input(event),
                WaylandClientWork::HostFocusLost(time) => self.release_host_input(time),
                WaylandClientWork::Presentation(claimant, update) => {
                    self.apply_presentation_claim(claimant, update)
                }
            }
        }
    }

    fn apply_client_request(&mut self, request: ClientRequest) {
        match request {
            ClientRequest::Surface(request) => match request.kind {
                ClientSurfaceRequestKind::Close => {
                    self.pending_resizes.discard(request.surface);
                    self.close_toplevel(request.surface);
                }
                ClientSurfaceRequestKind::Configure {
                    logical_size,
                    resizing,
                } => {
                    self.pending_resizes.queue(
                        request.surface,
                        PendingResize {
                            logical_size,
                            resizing,
                        },
                    );
                }
                ClientSurfaceRequestKind::SetOutputs {
                    outputs, preferred, ..
                } => {
                    let outputs = outputs
                        .into_iter()
                        .map(|output| OutputId::new(output.raw()))
                        .collect::<Vec<_>>();
                    self.set_toplevel_outputs(
                        request.surface,
                        &outputs,
                        preferred.map(|output| OutputId::new(output.raw())),
                    );
                }
                ClientSurfaceRequestKind::SetPreferredScale { scale_120 } => {
                    self.set_toplevel_preferred_scale(request.surface, scale_120);
                }
                ClientSurfaceRequestKind::SetPresentation { .. } => {
                    warn!(surface = ?request.surface, "presentation request requires an authorized presenter claim");
                }
            },
            ClientRequest::Focus(request) => {
                if request.source == crate::WAYLAND_CLIENT_SOURCE {
                    self.focus_toplevel(request.surface);
                }
            }
            ClientRequest::ClearFocus => {}
        }
    }

    /// Applies at most one pending configure per surface.
    pub(crate) fn flush_pending_resizes(&mut self) {
        let pending = self.pending_resizes.drain().collect::<Vec<_>>();
        for (surface, request) in pending {
            self.configure_toplevel(surface, request.logical_size, request.resizing);
        }
    }

    fn take_pending_resize(&mut self, surface: SurfaceId) -> Option<PendingResize> {
        self.pending_resizes.take(surface)
    }

    fn event_time(&self) -> u32 {
        self.started_at.elapsed().as_millis() as u32
    }

    fn allocate_client_id(&mut self) -> Option<ClientId> {
        let local = self.next_client_id?;
        self.next_client_id = local.checked_add(1);
        Some(ClientId::new(crate::WAYLAND_CLIENT_SOURCE, local))
    }
}

/// Host-only ingress. Its tree snapshots may still own Smithay DMA-BUFs.
#[derive(Debug)]
pub struct PendingSurfaceEvent {
    pub surface: SurfaceId,
    pub kind: PendingSurfaceEventKind,
}

#[derive(Debug)]
pub enum PendingSurfaceEventKind {
    Role(ClientSurfaceRole),
    Metadata(weld_client::ClientSurfaceMetadata),
    TreeSnapshot(surface_tree::PendingSurfaceTreeSnapshot),
    WindowInteraction(WindowInteractionRequestKind),
    Destroyed,
}

struct ClientState {
    compositor_state: CompositorClientState,
    id: ClientId,
}

impl ClientState {
    fn new(id: ClientId) -> Self {
        Self {
            compositor_state: CompositorClientState::default(),
            id,
        }
    }
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: WaylandClientId) {}

    fn disconnected(&self, _client_id: WaylandClientId, reason: DisconnectReason) {
        debug!(?reason, "Wayland client disconnected");
    }
}

smithay::delegate_dispatch2!(ServerState);
