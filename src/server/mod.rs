//! Smithay Wayland-server boundary shared by host backends.

mod dmabuf;
mod output;
mod popup;
mod seat;
mod shm;
mod surface_tree;
mod toplevel;

pub(crate) use output::{OutputDescriptor, OutputMetrics};
pub(crate) use surface_tree::{PendingSurfaceBufferContent, PendingSurfaceTreeSnapshot};

use std::{
    collections::{HashSet, VecDeque},
    ffi::OsString,
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result};
use smithay::{
    backend::allocator::dmabuf::DmabufSource,
    desktop::{PopupGrab, PopupManager},
    input::{Seat, SeatState},
    output::Output,
    reexports::{
        calloop::{
            Interest, LoopHandle, Mode, PostAction,
            channel::{Channel, Event as ChannelEvent},
            generic::Generic,
        },
        wayland_server::{
            Client, Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_callback::WlCallback,
        },
    },
    utils::Transform,
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::xdg::{XdgShellState, decoration::XdgDecorationState},
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
    },
};
use tracing::{debug, warn};

use crate::{
    dmabuf::{DmabufCapabilities, DmabufReleaseId, DmabufSourceCache},
    input::raw::InputPosition,
    surface::{
        AppPopup, HostSurfaceEvent, HostSurfaceEventKind, SurfaceAction, SurfaceId,
        WindowDecoration, WindowInteractionRequestKind,
    },
};
use dmabuf::{DmabufProtocol, DmabufReleaseStore};
use output::install_output_metrics;
use popup::PopupStore;
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
    shm_state: ShmState,
    dmabuf_protocol: DmabufProtocol,
    dmabuf_releases: DmabufReleaseStore,
    dmabuf_sources: DmabufSourceCache,
    _viewporter_state: ViewporterState,
    _fractional_scale_manager_state: FractionalScaleManagerState,
    _output_manager_state: OutputManagerState,
    seat_state: SeatState<Self>,
    data_device_state: DataDeviceState,
    seat: Seat<Self>,
    output: Output,
    output_metrics: OutputMetrics,
    toplevels: ToplevelStore,
    popups: PopupStore,
    popup_manager: PopupManager,
    popup_grab: Option<PopupGrab<Self>>,
    focused_toplevel: Option<SurfaceId>,
    pending_focus: Option<Option<SurfaceId>>,
    pending_surface_events: VecDeque<PendingSurfaceEvent>,
    presentation_requested: bool,
    next_presentation_id: u64,
    staged_frame_callbacks: VecDeque<(u64, Vec<WlCallback>)>,
    next_surface_id: Option<u64>,
    started_at: Instant,
    pointer_position: InputPosition,
    // This mirrors delivered presses only so host focus loss can synthesize
    // matching releases; ECS pointer routing remains the policy authority.
    pressed_pointer_buttons: HashSet<u32>,
    dmabuf_blocker_installer: Option<Box<dyn Fn(DmabufSource, Client) -> bool>>,
}

pub(crate) struct ServerOptions<'a> {
    pub(crate) started_at: Instant,
    pub(crate) seat_name: &'a str,
    pub(crate) output_descriptor: OutputDescriptor,
    pub(crate) output_metrics: OutputMetrics,
    pub(crate) dmabuf_capabilities: Option<&'a DmabufCapabilities>,
    pub(crate) dmabuf_sources: DmabufSourceCache,
}

impl ServerState {
    pub fn new<LoopData: 'static>(
        loop_handle: &LoopHandle<'static, LoopData>,
        display: Display<Self>,
        dmabuf_release_source: Channel<DmabufReleaseId>,
        server: fn(&mut LoopData) -> &mut Self,
        options: ServerOptions<'_>,
    ) -> Result<Self> {
        let ServerOptions {
            started_at,
            seat_name,
            output_descriptor,
            output_metrics,
            dmabuf_capabilities,
            dmabuf_sources,
        } = options;
        let display_handle = display.handle();
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
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

        let output = Output::new(
            output_descriptor.name,
            output_descriptor.physical_properties,
        );
        let output_mode = output_metrics.mode();
        output.create_global::<Self>(&display_handle);
        output.change_current_state(
            Some(output_mode),
            Some(Transform::Normal),
            Some(output_metrics.scale()),
            Some((0, 0).into()),
        );
        output.set_preferred(output_mode);

        let listening_socket = ListeningSocketSource::with_name(WELD_SOCKET_NAME)
            .with_context(|| {
                format!(
                    "failed to bind Weld Wayland socket {WELD_SOCKET_NAME:?}; another Weld instance may already be running"
                )
            })?;
        let socket_name = listening_socket.socket_name().to_os_string();
        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                let state = server(state);
                if let Err(error) = state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                {
                    warn!(%error, "rejected a Wayland client");
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
                if let ChannelEvent::Msg(release) = event {
                    server(state).complete_dmabuf_release(release);
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

        Ok(Self {
            display_handle,
            socket_name,
            compositor_state,
            xdg_shell_state,
            _xdg_decoration_state: xdg_decoration_state,
            shm_state,
            dmabuf_protocol,
            dmabuf_releases: DmabufReleaseStore::default(),
            dmabuf_sources,
            _viewporter_state: viewporter_state,
            _fractional_scale_manager_state: fractional_scale_manager_state,
            _output_manager_state: output_manager_state,
            seat_state,
            data_device_state,
            seat,
            output,
            output_metrics,
            toplevels: ToplevelStore::default(),
            popups: PopupStore::default(),
            popup_manager: PopupManager::default(),
            popup_grab: None,
            focused_toplevel: None,
            pending_focus: None,
            pending_surface_events: VecDeque::new(),
            presentation_requested: false,
            next_presentation_id: 1,
            staged_frame_callbacks: VecDeque::new(),
            next_surface_id: Some(1),
            started_at,
            pointer_position: InputPosition::default(),
            pressed_pointer_buttons: HashSet::new(),
            dmabuf_blocker_installer,
        })
    }

    pub(crate) fn update_output_metrics(&mut self, metrics: OutputMetrics) {
        if self.output_metrics == metrics {
            return;
        }
        install_output_metrics(&self.output, self.output_metrics, metrics);
        self.output_metrics = metrics;
        self.send_all_surface_scales();
    }

    pub fn take_surface_events(&mut self) -> impl Iterator<Item = PendingSurfaceEvent> + '_ {
        self.pending_surface_events.drain(..)
    }

    pub(crate) fn complete_dmabuf_release(&mut self, release: DmabufReleaseId) {
        self.dmabuf_releases.complete(release);
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

    pub fn apply_surface_action(&mut self, action: SurfaceAction) {
        match action {
            SurfaceAction::Close { surface } => self.close_toplevel(surface),
            SurfaceAction::Focus { surface } => self.focus_toplevel(surface),
            SurfaceAction::Resize {
                surface,
                logical_size,
            } => self.resize_toplevel(surface, logical_size),
        }
    }

    fn event_time(&self) -> u32 {
        self.started_at.elapsed().as_millis() as u32
    }
}

/// Host-only ingress. Its tree snapshots may still own Smithay DMA-BUFs.
#[derive(Debug)]
pub(crate) struct PendingSurfaceEvent {
    pub(crate) surface: SurfaceId,
    pub(crate) kind: PendingSurfaceEventKind,
}

#[derive(Debug)]
pub(crate) enum PendingSurfaceEventKind {
    Created { decoration: WindowDecoration },
    TreeSnapshot(surface_tree::PendingSurfaceTreeSnapshot),
    DecorationChanged { decoration: WindowDecoration },
    PopupConfigured(AppPopup),
    WindowInteraction(WindowInteractionRequestKind),
    Destroyed,
}

impl PendingSurfaceEvent {
    pub(crate) fn without_tree_payload(self) -> Option<HostSurfaceEvent> {
        let Self { surface, kind } = self;
        let kind = match kind {
            PendingSurfaceEventKind::Created { decoration } => {
                HostSurfaceEventKind::Created { decoration }
            }
            PendingSurfaceEventKind::DecorationChanged { decoration } => {
                HostSurfaceEventKind::DecorationChanged { decoration }
            }
            PendingSurfaceEventKind::PopupConfigured(popup) => {
                HostSurfaceEventKind::PopupConfigured(popup)
            }
            PendingSurfaceEventKind::WindowInteraction(request) => {
                HostSurfaceEventKind::WindowInteraction(request)
            }
            PendingSurfaceEventKind::Destroyed => HostSurfaceEventKind::Destroyed,
            PendingSurfaceEventKind::TreeSnapshot(_) => return None,
        };
        Some(HostSurfaceEvent { surface, kind })
    }
}

#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}

    fn disconnected(&self, _client_id: ClientId, reason: DisconnectReason) {
        debug!(?reason, "Wayland client disconnected");
    }
}

smithay::delegate_dispatch2!(ServerState);
