//! Smithay host boundary for the nested compositor.

mod output;
mod seat;
mod shm;
mod surface_tree;
mod toplevel;

pub(crate) use output::NestedOutputMetrics;

use std::{collections::HashSet, ffi::OsString, sync::Arc, time::Instant};

use anyhow::{Context, Result};
use smithay::{
    input::{Seat, SeatState},
    output::{Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic},
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
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
    raw_input::InputPosition,
    surface::{HostSurfaceEvent, SurfaceAction, SurfaceEventQueue, SurfaceId},
};
use output::install_output_metrics;
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
    _viewporter_state: ViewporterState,
    _fractional_scale_manager_state: FractionalScaleManagerState,
    _output_manager_state: OutputManagerState,
    seat_state: SeatState<Self>,
    data_device_state: DataDeviceState,
    seat: Seat<Self>,
    output: Output,
    output_metrics: NestedOutputMetrics,
    toplevels: ToplevelStore,
    focused_toplevel: Option<SurfaceId>,
    pending_focus: Option<Option<SurfaceId>>,
    pending_surface_events: SurfaceEventQueue,
    presentation_requested: bool,
    next_surface_id: Option<u64>,
    started_at: Instant,
    pointer_position: InputPosition,
    // This mirrors delivered presses only so host focus loss can synthesize
    // matching releases; ECS pointer routing remains the policy authority.
    pressed_pointer_buttons: HashSet<u32>,
}

impl ServerState {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        started_at: Instant,
        output_metrics: NestedOutputMetrics,
    ) -> Result<Self> {
        let display_handle = display.handle();
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, []);
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);
        let fractional_scale_manager_state =
            FractionalScaleManagerState::new::<Self>(&display_handle);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "weld-nested");
        seat.add_keyboard(Default::default(), 200, 25)
            .context("failed to initialize the nested keyboard keymap")?;
        seat.add_pointer();

        let output = Output::new(
            "weld-nested".to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Weld".to_owned(),
                model: "Nested".to_owned(),
                serial_number: "development".to_owned(),
            },
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
        let loop_handle = event_loop.handle();

        loop_handle
            .insert_source(listening_socket, |client_stream, _, state| {
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
                |_, display, state| {
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

        Ok(Self {
            display_handle,
            socket_name,
            compositor_state,
            xdg_shell_state,
            _xdg_decoration_state: xdg_decoration_state,
            shm_state,
            _viewporter_state: viewporter_state,
            _fractional_scale_manager_state: fractional_scale_manager_state,
            _output_manager_state: output_manager_state,
            seat_state,
            data_device_state,
            seat,
            output,
            output_metrics,
            toplevels: ToplevelStore::default(),
            focused_toplevel: None,
            pending_focus: None,
            pending_surface_events: SurfaceEventQueue::default(),
            presentation_requested: false,
            next_surface_id: Some(1),
            started_at,
            pointer_position: InputPosition::default(),
            pressed_pointer_buttons: HashSet::new(),
        })
    }

    pub(crate) fn update_output_metrics(&mut self, metrics: NestedOutputMetrics) {
        if self.output_metrics == metrics {
            return;
        }
        install_output_metrics(&self.output, self.output_metrics, metrics);
        self.output_metrics = metrics;
        self.send_all_surface_scales();
    }

    pub fn take_surface_events(&mut self) -> impl Iterator<Item = HostSurfaceEvent> + '_ {
        self.pending_surface_events.drain()
    }

    pub const fn presentation_requested(&self) -> bool {
        self.presentation_requested
    }

    pub fn frame_presented(&mut self) {
        // No protocol dispatch occurs between composing this frame and acknowledging it, so
        // clearing the request cannot discard a newer client commit.
        self.presentation_requested = false;
        self.complete_surface_presentation();
    }

    pub fn flush_clients(&mut self) {
        if let Err(error) = self.display_handle.flush_clients() {
            warn!(%error, "failed to flush Wayland clients");
        }
    }

    pub fn apply_surface_action(&mut self, action: SurfaceAction) {
        match action {
            SurfaceAction::Close { surface } => self.close_toplevel(surface),
            SurfaceAction::Focus { surface } => self.focus_toplevel(surface),
        }
    }

    fn event_time(&self) -> u32 {
        self.started_at.elapsed().as_millis() as u32
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
