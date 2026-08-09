//! Minimal Smithay host for the nested SHM rendering experiment.

use std::{collections::HashSet, ffi::OsString, sync::Arc, time::Instant};

use anyhow::{Context, Result, anyhow, bail};
use bevy::input::ButtonState as BevyButtonState;
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState as SmithayButtonState, KeyState, Keycode},
    input::{
        Seat, SeatHandler, SeatState,
        dnd::DndGrabHandler,
        keyboard::{FilterResult, KeyboardSource},
        pointer::{AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent},
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic},
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer, wl_output, wl_seat, wl_shm, wl_surface::WlSurface},
        },
    },
    utils::{Buffer as BufferCoord, Logical, Rectangle, SERIAL_COUNTER, Serial, Size, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, send_surface_state, with_states,
        },
        fractional_scale::{
            FractionalScaleHandler, FractionalScaleManagerState, with_fractional_scale,
        },
        output::{OutputHandler, OutputManagerState},
        selection::{
            SelectionHandler,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        shm::{BufferData, ShmHandler, ShmState, with_buffer_contents},
        socket::ListeningSocketSource,
        viewporter::{ViewportCachedState, ViewporterState, ensure_viewport_valid},
    },
};
use tracing::{debug, info, warn};

use crate::{
    compositor::{
        HostSurfaceEvent, SurfaceContentView, SurfaceEventQueue, SurfaceFrame, SurfaceId,
    },
    input::{SeatInputEffect, SeatInputEffectKind, SurfaceHit},
    raw_input::{InputPosition, RawScrollFrame, RawScrollSource},
    window::SurfaceAction,
};

// Keep this stable name in sync with scripts/run-app.
const WELD_SOCKET_NAME: &str = "weld-0";

const CLIENT_WIDTH: i32 = 640;
const CLIENT_HEIGHT: i32 = 480;

/// Physical host extent plus the effective logical scale advertised to nested
/// clients. A future configuration layer can select the scale before creating
/// this Winit-independent boundary value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NestedOutputMetrics {
    physical_width: i32,
    physical_height: i32,
    scale_factor: f64,
}

impl NestedOutputMetrics {
    pub(crate) fn new(
        physical_width: u32,
        physical_height: u32,
        scale_factor: f64,
    ) -> Result<Self> {
        if physical_width == 0 || physical_height == 0 {
            bail!("nested output dimensions must be nonzero");
        }
        if !scale_factor.is_finite() || scale_factor <= 0.0 {
            bail!("nested output scale must be finite and positive");
        }
        Ok(Self {
            physical_width: i32::try_from(physical_width)
                .context("nested output width exceeds i32")?,
            physical_height: i32::try_from(physical_height)
                .context("nested output height exceeds i32")?,
            scale_factor,
        })
    }

    fn mode(self) -> OutputMode {
        OutputMode {
            size: (self.physical_width, self.physical_height).into(),
            refresh: 60_000,
        }
    }

    fn scale(self) -> Scale {
        Scale::Fractional(self.scale_factor)
    }

    pub(crate) const fn scale_factor(self) -> f64 {
        self.scale_factor
    }

    pub(crate) const fn physical_width(self) -> u32 {
        self.physical_width as u32
    }

    pub(crate) const fn physical_height(self) -> u32 {
        self.physical_height as u32
    }
}

struct ActiveToplevel {
    surface: ToplevelSurface,
    id: SurfaceId,
    buffer: Option<SurfaceBufferMetadata>,
    view: Option<SurfaceContentView>,
}

#[derive(Clone, Copy, Debug)]
struct SurfaceBufferMetadata {
    width: u32,
    height: u32,
    scale: u32,
    transform: wl_output::Transform,
}

/// Smithay state kept outside Bevy's ECS world.
pub struct ServerState {
    pub display_handle: DisplayHandle,
    pub socket_name: OsString,
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    _viewporter_state: ViewporterState,
    _fractional_scale_manager_state: FractionalScaleManagerState,
    _output_manager_state: OutputManagerState,
    seat_state: SeatState<Self>,
    data_device_state: DataDeviceState,
    seat: Seat<Self>,
    output: Output,
    output_metrics: NestedOutputMetrics,
    active_toplevel: Option<ActiveToplevel>,
    pending_surface_events: SurfaceEventQueue,
    presentation_requested: bool,
    next_surface_id: u64,
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
            shm_state,
            _viewporter_state: viewporter_state,
            _fractional_scale_manager_state: fractional_scale_manager_state,
            _output_manager_state: output_manager_state,
            seat_state,
            data_device_state,
            seat,
            output,
            output_metrics,
            active_toplevel: None,
            pending_surface_events: SurfaceEventQueue::default(),
            presentation_requested: false,
            next_surface_id: 1,
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
        self.send_active_surface_scale();
    }

    fn send_active_surface_scale(&self) {
        let Some(surface) = self
            .active_toplevel
            .as_ref()
            .map(|active| active.surface.wl_surface())
        else {
            return;
        };
        send_preferred_surface_scale(&self.output, surface);
    }

    pub fn take_surface_events(&mut self) -> impl Iterator<Item = HostSurfaceEvent> + '_ {
        self.pending_surface_events.drain()
    }

    pub const fn presentation_requested(&self) -> bool {
        self.presentation_requested
    }

    pub fn frame_presented(&mut self) {
        // The host calls this only after presenting a completed, clean composition. No
        // protocol dispatch occurs between that composition and this acknowledgement,
        // so clearing the request cannot discard a newer client commit.
        self.presentation_requested = false;
        let Some(toplevel) = self
            .active_toplevel
            .as_ref()
            .filter(|toplevel| toplevel.surface.alive())
        else {
            return;
        };
        let time = self.started_at.elapsed().as_millis() as u32;
        with_states(toplevel.surface.wl_surface(), |states| {
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            for callback in attributes.current().frame_callbacks.drain(..) {
                callback.done(time);
            }
        });
    }

    pub fn flush_clients(&mut self) {
        if let Err(error) = self.display_handle.flush_clients() {
            warn!(%error, "failed to flush Wayland clients");
        }
    }

    pub fn apply_input_effect(&mut self, effect: SeatInputEffect) {
        let SeatInputEffect { event, time } = effect;
        match event {
            SeatInputEffectKind::PointerMotion { position, target } => {
                self.apply_pointer_motion(position, target, time)
            }
            SeatInputEffectKind::PointerButton {
                position,
                target,
                button,
                state,
            } => self.apply_pointer_button(position, target, button.0, state, time),
            SeatInputEffectKind::PointerAxis {
                position,
                target,
                axis,
            } => self.apply_pointer_axis(position, target, axis, time),
            SeatInputEffectKind::Keyboard { keycode, state } => {
                let Some(keycode) = keycode.0.checked_add(8) else {
                    warn!(keycode = keycode.0, "ignored an overflowing keyboard code");
                    return;
                };
                let Some(keyboard) = self.seat.get_keyboard() else {
                    warn!("ignored keyboard input because the seat has no keyboard");
                    return;
                };
                keyboard.input::<(), _>(
                    self,
                    Keycode::new(keycode),
                    smithay_key_state(state),
                    SERIAL_COUNTER.next_serial(),
                    time,
                    // Future global-shortcut arbitration belongs here. Press and
                    // release suppression must remain paired when interception is added.
                    |_, _, _| FilterResult::Forward,
                );
            }
            SeatInputEffectKind::HostFocusLost => self.release_host_input(time),
        }
    }

    pub fn apply_surface_action(&mut self, action: SurfaceAction) {
        match action {
            SurfaceAction::Close { surface } => {
                let Some(toplevel) = self
                    .active_toplevel
                    .as_ref()
                    .filter(|toplevel| toplevel.id == surface)
                else {
                    warn!(?surface, "ignored a close request for an unknown surface");
                    return;
                };
                toplevel.surface.send_close();
            }
        }
    }

    fn apply_pointer_motion(
        &mut self,
        position: InputPosition,
        target: Option<SurfaceHit>,
        time: u32,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            warn!("ignored pointer motion because the seat has no pointer");
            return;
        };
        self.pointer_position = position;
        let focus = self.pointer_focus(position, target);
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: compositor_point(position),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        // Pointer protocol events are buffered by v5+ clients until the frame.
        pointer.frame(self);
    }

    fn apply_pointer_button(
        &mut self,
        position: InputPosition,
        target: Option<SurfaceHit>,
        button: u32,
        state: BevyButtonState,
        time: u32,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            warn!("ignored pointer button because the seat has no pointer");
            return;
        };
        self.pointer_position = position;
        let serial = SERIAL_COUNTER.next_serial();
        let focus = self.pointer_focus(position, target);
        let keyboard_focus = focus.as_ref().map(|(surface, _)| surface.clone());
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: compositor_point(position),
                serial,
                time,
            },
        );
        if state == BevyButtonState::Pressed
            && !pointer.is_grabbed()
            && let Some(keyboard) = self.seat.get_keyboard()
        {
            keyboard.set_focus(self, keyboard_focus, serial);
        }
        match state {
            BevyButtonState::Pressed => {
                self.pressed_pointer_buttons.insert(button);
            }
            BevyButtonState::Released => {
                self.pressed_pointer_buttons.remove(&button);
            }
        }
        pointer.button(
            self,
            &ButtonEvent {
                serial,
                time,
                button,
                state: smithay_button_state(state),
            },
        );
        pointer.frame(self);
    }

    fn apply_pointer_axis(
        &mut self,
        position: InputPosition,
        target: Option<SurfaceHit>,
        axis: RawScrollFrame,
        time: u32,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            warn!("ignored pointer axis because the seat has no pointer");
            return;
        };
        self.pointer_position = position;
        let focus = self.pointer_focus(position, target);
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: compositor_point(position),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        if let Some(frame) = smithay_axis_frame(axis, time) {
            pointer.axis(self, frame);
        }
        pointer.frame(self);
    }

    fn pointer_focus(
        &self,
        position: InputPosition,
        target: Option<SurfaceHit>,
    ) -> Option<(WlSurface, smithay::utils::Point<f64, Logical>)> {
        let target = target?;
        let active = self
            .active_toplevel
            .as_ref()
            .filter(|active| active.id == target.surface && active.surface.alive())?;
        let origin = InputPosition::new(
            position.x - target.local_position.x,
            position.y - target.local_position.y,
        );
        Some((
            active.surface.wl_surface().clone(),
            compositor_point(origin),
        ))
    }

    fn release_host_input(&mut self, time: u32) {
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(pointer) = self.seat.get_pointer() {
            for button in std::mem::take(&mut self.pressed_pointer_buttons) {
                pointer.button(
                    self,
                    &ButtonEvent {
                        serial,
                        time,
                        button,
                        state: SmithayButtonState::Released,
                    },
                );
            }
            pointer.motion(
                self,
                None,
                &MotionEvent {
                    location: compositor_point(self.pointer_position),
                    serial,
                    time,
                },
            );
            pointer.frame(self);
        } else {
            self.pressed_pointer_buttons.clear();
            warn!("could not release host pointer state because the seat has no pointer");
        }

        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.release_source(self, KeyboardSource::MAIN);
            keyboard.set_focus(self, None, serial);
        }
    }

    fn clear_input_focus_for_surface(&mut self, surface: &WlSurface, time: u32) {
        // SurfaceId values are never reused. If that changes, the Bevy bridge's
        // cached hit must also be invalidated when this host-side focus is cleared.
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(pointer) = self.seat.get_pointer()
            && pointer.current_focus().as_ref() == Some(surface)
        {
            pointer.motion(
                self,
                None,
                &MotionEvent {
                    location: compositor_point(self.pointer_position),
                    serial,
                    time,
                },
            );
            pointer.frame(self);
        }
        if let Some(keyboard) = self.seat.get_keyboard()
            && keyboard.current_focus().as_ref() == Some(surface)
        {
            keyboard.set_focus(self, None, serial);
        }
    }

    fn event_time(&self) -> u32 {
        self.started_at.elapsed().as_millis() as u32
    }

    fn handle_root_commit(&mut self, surface: &WlSurface) {
        let Some((surface_id, retained_buffer, previous_view)) = self
            .active_toplevel
            .as_ref()
            .map(|toplevel| (toplevel.id, toplevel.buffer, toplevel.view))
        else {
            return;
        };
        let (assignment, buffer_scale, buffer_transform) = with_states(surface, |states| {
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            let current = attributes.current();
            (
                current.buffer.take(),
                current.buffer_scale,
                current.buffer_transform,
            )
        });

        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let copied = copy_shm_buffer(&buffer).and_then(|copied| {
                    let metadata = SurfaceBufferMetadata {
                        width: copied.width,
                        height: copied.height,
                        scale: checked_buffer_scale(buffer_scale)?,
                        transform: buffer_transform,
                    };
                    let view = surface_content_view(surface, metadata)?;
                    Ok((copied, metadata, view))
                });
                buffer.release();
                match copied {
                    Ok((copied, metadata, view)) => {
                        if let Some(active) = self.active_toplevel.as_mut() {
                            active.buffer = Some(metadata);
                            active.view = Some(view);
                        }
                        self.pending_surface_events.push(HostSurfaceEvent::Frame {
                            surface: surface_id,
                            frame: SurfaceFrame {
                                width: copied.width,
                                height: copied.height,
                                view,
                                bgra_pixels: copied.bgra_pixels,
                                opaque: copied.opaque,
                            },
                        });
                    }
                    Err(error) => {
                        if let Some(active) = self.active_toplevel.as_mut() {
                            active.buffer = None;
                            active.view = None;
                        }
                        warn!(%error, "ignored an unsupported client buffer");
                    }
                }
            }
            Some(BufferAssignment::Removed) => {
                if let Some(active) = self.active_toplevel.as_mut() {
                    active.buffer = None;
                    active.view = None;
                }
                self.clear_input_focus_for_surface(surface, self.event_time());
                self.pending_surface_events
                    .push(HostSurfaceEvent::Unmapped {
                        surface: surface_id,
                    });
            }
            None => {
                let Some(retained_buffer) = retained_buffer else {
                    return;
                };
                let metadata = match checked_buffer_scale(buffer_scale) {
                    Ok(scale) => SurfaceBufferMetadata {
                        scale,
                        transform: buffer_transform,
                        ..retained_buffer
                    },
                    Err(error) => {
                        warn!(%error, "ignored an invalid client surface scale");
                        return;
                    }
                };
                match surface_content_view(surface, metadata) {
                    Ok(view) if previous_view != Some(view) => {
                        if let Some(active) = self.active_toplevel.as_mut() {
                            active.buffer = Some(metadata);
                            active.view = Some(view);
                        }
                        self.pending_surface_events
                            .push(HostSurfaceEvent::ViewChanged {
                                surface: surface_id,
                                view,
                            });
                    }
                    Ok(_) => {
                        if let Some(active) = self.active_toplevel.as_mut() {
                            active.buffer = Some(metadata);
                        }
                    }
                    Err(error) => warn!(%error, "ignored an invalid client surface view"),
                }
            }
        }
    }

    fn allocate_surface_id(&mut self) -> SurfaceId {
        let id = SurfaceId::new(self.next_surface_id);
        self.next_surface_id = if self.next_surface_id == u64::MAX {
            1
        } else {
            self.next_surface_id + 1
        };
        id
    }
}

fn install_output_metrics(
    output: &Output,
    previous: NestedOutputMetrics,
    next: NestedOutputMetrics,
) {
    let previous_mode = previous.mode();
    let next_mode = next.mode();
    output.change_current_state(Some(next_mode), None, Some(next.scale()), None);
    output.set_preferred(next_mode);
    if previous_mode != next_mode {
        // Smithay retains every installed mode for future wl_output binds.
        // Retire the old mode only after current/preferred point at the new one.
        output.delete_mode(previous_mode);
    }
}

fn send_preferred_surface_scale(output: &Output, surface: &WlSurface) {
    let output_scale = output.current_scale();
    with_states(surface, |states| {
        send_surface_state(
            surface,
            states,
            output_scale.integer_scale(),
            Transform::Normal,
        );
        with_fractional_scale(states, |fractional_scale| {
            fractional_scale.set_preferred_scale(output_scale.fractional_scale());
        });
    });
}

impl BufferHandler for ServerState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for ServerState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl FractionalScaleHandler for ServerState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // The initial slice tracks only the active root across later output
        // changes. Every surface still receives the current scale when it
        // creates its fractional-scale object.
        send_preferred_surface_scale(&self.output, &surface);
    }
}

impl CompositorHandler for ServerState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // Every accepted client is inserted with ClientState in ServerState::new. Smithay's
        // handler trait cannot express that invariant as a fallible return value.
        &client
            .get_data::<ClientState>()
            .expect("Weld inserts ClientState for every accepted client")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        let is_active_root = self
            .active_toplevel
            .as_ref()
            .is_some_and(|toplevel| toplevel.surface.wl_surface() == surface);
        if is_active_root {
            // A callback-only commit still needs a presentation acknowledgement, even when
            // there is no new buffer for Bevy to compose.
            self.presentation_requested = true;
            self.handle_root_commit(surface);
        } else {
            debug!(surface = ?surface.id(), "ignoring a non-root surface commit in the initial slice");
        }
    }
}

impl XdgShellHandler for ServerState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        if let Some(previous) = self.active_toplevel.take() {
            self.clear_input_focus_for_surface(previous.surface.wl_surface(), self.event_time());
            warn!("replacing the active toplevel; the initial slice displays one client at a time");
            self.pending_surface_events
                .push(HostSurfaceEvent::Destroyed {
                    surface: previous.id,
                });
        }
        surface.with_pending_state(|state| {
            state.size = Some(Size::<i32, Logical>::from((CLIENT_WIDTH, CLIENT_HEIGHT)));
            state.states.set(xdg_toplevel::State::Activated);
        });
        self.output.enter(surface.wl_surface());
        send_preferred_surface_scale(&self.output, surface.wl_surface());
        surface.send_configure();
        let id = self.allocate_surface_id();
        self.pending_surface_events
            .push(HostSurfaceEvent::Created { surface: id });
        info!(surface_id = id.raw(), "created a nested xdg-toplevel");
        self.active_toplevel = Some(ActiveToplevel {
            surface,
            id,
            buffer: None,
            view: None,
        });
        self.presentation_requested = true;
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {
        debug!("ignoring an xdg-popup in the initial slice");
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let is_active = self
            .active_toplevel
            .as_ref()
            .is_some_and(|active| active.surface.wl_surface() == surface.wl_surface());
        if is_active && let Some(active) = self.active_toplevel.take() {
            self.clear_input_focus_for_surface(active.surface.wl_surface(), self.event_time());
            self.pending_surface_events
                .push(HostSurfaceEvent::Destroyed { surface: active.id });
        }
    }
}

impl OutputHandler for ServerState {}

impl SeatHandler for ServerState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|surface| self.display_handle.get_client(surface.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}

impl SelectionHandler for ServerState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for ServerState {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DndGrabHandler for ServerState {}
impl WaylandDndGrabHandler for ServerState {}

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

fn compositor_point(position: InputPosition) -> smithay::utils::Point<f64, Logical> {
    (position.x, position.y).into()
}

const fn smithay_key_state(state: BevyButtonState) -> KeyState {
    match state {
        BevyButtonState::Pressed => KeyState::Pressed,
        BevyButtonState::Released => KeyState::Released,
    }
}

const fn smithay_button_state(state: BevyButtonState) -> SmithayButtonState {
    match state {
        BevyButtonState::Pressed => SmithayButtonState::Pressed,
        BevyButtonState::Released => SmithayButtonState::Released,
    }
}

fn smithay_axis_frame(axis: RawScrollFrame, time: u32) -> Option<AxisFrame> {
    if axis.horizontal == 0.0
        && axis.vertical == 0.0
        && axis.horizontal_v120.unwrap_or_default() == 0
        && axis.vertical_v120.unwrap_or_default() == 0
    {
        return None;
    }
    let source = match axis.source {
        RawScrollSource::Wheel => AxisSource::Wheel,
        RawScrollSource::Continuous => AxisSource::Continuous,
    };
    let mut frame = AxisFrame::new(time).source(source);
    if axis.horizontal != 0.0 {
        frame = frame.value(Axis::Horizontal, axis.horizontal);
    }
    if axis.vertical != 0.0 {
        frame = frame.value(Axis::Vertical, axis.vertical);
    }
    if let Some(v120) = axis.horizontal_v120
        && v120 != 0
    {
        frame = frame.v120(Axis::Horizontal, v120);
    }
    if let Some(v120) = axis.vertical_v120
        && v120 != 0
    {
        frame = frame.v120(Axis::Vertical, v120);
    }
    Some(frame)
}

struct CopiedShmBuffer {
    width: u32,
    height: u32,
    bgra_pixels: Vec<u8>,
    opaque: bool,
}

fn copy_shm_buffer(buffer: &wl_buffer::WlBuffer) -> Result<CopiedShmBuffer> {
    if !cfg!(target_endian = "little") {
        bail!("the initial BGRA upload path requires a little-endian target");
    }

    with_buffer_contents(buffer, |pointer, pool_length, data| {
        copy_shm_contents(pointer, pool_length, data)
    })
    .context("buffer is not readable Wayland SHM")?
}

fn copy_shm_contents(
    pointer: *const u8,
    pool_length: usize,
    data: BufferData,
) -> Result<CopiedShmBuffer> {
    let width = usize::try_from(data.width).context("negative SHM width")?;
    let height = usize::try_from(data.height).context("negative SHM height")?;
    let stride = usize::try_from(data.stride).context("negative SHM stride")?;
    let offset = usize::try_from(data.offset).context("negative SHM offset")?;
    if width == 0 || height == 0 {
        bail!("zero-sized SHM buffer");
    }

    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| anyhow!("SHM row size overflow"))?;
    if stride < row_bytes {
        bail!("SHM stride is shorter than one pixel row");
    }
    let span = stride
        .checked_mul(height - 1)
        .and_then(|prefix| prefix.checked_add(row_bytes))
        .ok_or_else(|| anyhow!("SHM buffer size overflow"))?;
    let end = offset
        .checked_add(span)
        .ok_or_else(|| anyhow!("SHM pool offset overflow"))?;
    if end > pool_length {
        bail!("SHM buffer extends beyond its pool");
    }

    // SAFETY: Smithay guarantees the pointer is valid for pool_length bytes during this
    // callback. The Wayland client must not mutate an attached buffer until wl_buffer.release;
    // Weld copies it synchronously and releases it only after this callback returns.
    let source = unsafe { std::slice::from_raw_parts(pointer.add(offset), span) };
    let pixels = normalize_bgra_rows(source, width, height, stride, data.format)?;

    Ok(CopiedShmBuffer {
        width: u32::try_from(width).context("SHM width exceeds u32")?,
        height: u32::try_from(height).context("SHM height exceeds u32")?,
        bgra_pixels: pixels,
        opaque: data.format == wl_shm::Format::Xrgb8888,
    })
}

fn surface_content_view(
    surface: &WlSurface,
    metadata: SurfaceBufferMetadata,
) -> Result<SurfaceContentView> {
    if metadata.transform != wl_output::Transform::Normal {
        bail!(
            "unsupported client buffer transform {:?}; the initial SHM path supports only normal",
            metadata.transform
        );
    }
    let width = i32::try_from(metadata.width).context("client buffer width exceeds i32")?;
    let height = i32::try_from(metadata.height).context("client buffer height exceeds i32")?;
    let scale = i32::try_from(metadata.scale).context("client buffer scale exceeds i32")?;
    let logical_buffer_size =
        Size::<i32, BufferCoord>::from((width, height)).to_logical(scale, Transform::Normal);

    with_states(surface, |states| {
        if !ensure_viewport_valid(states, logical_buffer_size) {
            bail!("client viewport source extends outside its buffer");
        }
        let viewport = {
            let mut cached = states.cached_state.get::<ViewportCachedState>();
            *cached.current()
        };
        translate_surface_content_view(metadata, logical_buffer_size, viewport)
    })
}

fn translate_surface_content_view(
    metadata: SurfaceBufferMetadata,
    logical_buffer_size: Size<i32, Logical>,
    viewport: ViewportCachedState,
) -> Result<SurfaceContentView> {
    if metadata.transform != wl_output::Transform::Normal {
        bail!("only normal client buffer transforms can be translated");
    }
    let full_source = Rectangle::from_size(logical_buffer_size.to_f64());
    let source = viewport.src.unwrap_or(full_source);
    let destination = viewport.size().unwrap_or(logical_buffer_size);
    let source_right = source.loc.x + source.size.w;
    let source_bottom = source.loc.y + source.size.h;
    if !source.loc.x.is_finite()
        || !source.loc.y.is_finite()
        || !source.size.w.is_finite()
        || !source.size.h.is_finite()
        || source.loc.x < 0.0
        || source.loc.y < 0.0
        || source.size.w <= 0.0
        || source.size.h <= 0.0
        || source_right > f64::from(logical_buffer_size.w)
        || source_bottom > f64::from(logical_buffer_size.h)
        || destination.w <= 0
        || destination.h <= 0
    {
        bail!("invalid client surface viewport geometry");
    }

    let scale = f64::from(metadata.scale);
    let view = SurfaceContentView {
        source_x: (source.loc.x * scale) as f32,
        source_y: (source.loc.y * scale) as f32,
        source_width: (source.size.w * scale) as f32,
        source_height: (source.size.h * scale) as f32,
        logical_width: destination.w as f32,
        logical_height: destination.h as f32,
    };
    let values = [
        view.source_x,
        view.source_y,
        view.source_width,
        view.source_height,
        view.logical_width,
        view.logical_height,
    ];
    if !values.into_iter().all(f32::is_finite)
        || view.source_x + view.source_width > metadata.width as f32
        || view.source_y + view.source_height > metadata.height as f32
    {
        bail!("client surface viewport cannot be represented by the SHM image path");
    }
    Ok(view)
}

fn checked_buffer_scale(buffer_scale: i32) -> Result<u32> {
    u32::try_from(buffer_scale)
        .ok()
        .filter(|scale| *scale > 0)
        .ok_or_else(|| anyhow!("invalid client buffer scale {buffer_scale}"))
}

fn normalize_bgra_rows(
    source: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    format: wl_shm::Format,
) -> Result<Vec<u8>> {
    if !matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888) {
        bail!("unsupported SHM format {format:?}");
    }
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| anyhow!("SHM row size overflow"))?;
    let required = stride
        .checked_mul(height.saturating_sub(1))
        .and_then(|prefix| prefix.checked_add(row_bytes))
        .ok_or_else(|| anyhow!("SHM source size overflow"))?;
    if stride < row_bytes || source.len() < required {
        bail!("invalid SHM row layout");
    }

    let mut pixels = Vec::with_capacity(
        row_bytes
            .checked_mul(height)
            .ok_or_else(|| anyhow!("SHM destination size overflow"))?,
    );
    for row in 0..height {
        let start = row * stride;
        pixels.extend_from_slice(&source[start..start + row_bytes]);
    }
    if format == wl_shm::Format::Xrgb8888 {
        for alpha in pixels[3..].iter_mut().step_by(4) {
            *alpha = u8::MAX;
        }
    }
    Ok(pixels)
}

smithay::delegate_dispatch2!(ServerState);

#[cfg(test)]
mod tests {
    use super::{
        NestedOutputMetrics, SurfaceBufferMetadata, checked_buffer_scale, install_output_metrics,
        normalize_bgra_rows, smithay_axis_frame, translate_surface_content_view,
    };
    use crate::compositor::SurfaceContentView;
    use crate::raw_input::{RawScrollFrame, RawScrollSource};
    use smithay::output::{Output, PhysicalProperties, Subpixel};
    use smithay::reexports::wayland_server::protocol::{wl_output, wl_shm};
    use smithay::utils::{Logical, Rectangle, Size, Transform};
    use smithay::wayland::viewporter::ViewportCachedState;

    fn metadata(width: u32, height: u32, scale: u32) -> SurfaceBufferMetadata {
        SurfaceBufferMetadata {
            width,
            height,
            scale,
            transform: wl_output::Transform::Normal,
        }
    }

    #[test]
    fn nested_output_metrics_preserve_physical_mode_and_fractional_scale() {
        let metrics = NestedOutputMetrics::new(1200, 800, 1.25).expect("valid output metrics");
        assert_eq!(metrics.mode().size, (1200, 800).into());
        assert_eq!(metrics.scale().fractional_scale(), 1.25);
        assert_eq!(metrics.scale().integer_scale(), 2);
        assert_eq!(metrics.scale_factor(), 1.25);
    }

    #[test]
    fn nested_output_metrics_reject_invalid_values() {
        assert!(NestedOutputMetrics::new(0, 800, 1.25).is_err());
        assert!(NestedOutputMetrics::new(1200, 800, 0.0).is_err());
        assert!(NestedOutputMetrics::new(1200, 800, f64::NAN).is_err());
    }

    #[test]
    fn replacing_nested_metrics_keeps_one_current_preferred_mode() {
        let initial = NestedOutputMetrics::new(1200, 800, 1.25).unwrap();
        let resized = NestedOutputMetrics::new(1300, 900, 1.25).unwrap();
        let resized_again = NestedOutputMetrics::new(1400, 1000, 1.25).unwrap();
        let scale_only = NestedOutputMetrics::new(1400, 1000, 1.5).unwrap();
        let output = Output::new(
            "test".to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Weld".to_owned(),
                model: "Test".to_owned(),
                serial_number: "test".to_owned(),
            },
        );
        output.change_current_state(
            Some(initial.mode()),
            Some(Transform::Normal),
            Some(initial.scale()),
            Some((0, 0).into()),
        );
        output.set_preferred(initial.mode());

        install_output_metrics(&output, initial, resized);
        install_output_metrics(&output, resized, resized_again);
        assert_eq!(output.modes(), [resized_again.mode()]);
        assert_eq!(output.current_mode(), Some(resized_again.mode()));
        assert_eq!(output.preferred_mode(), Some(resized_again.mode()));

        install_output_metrics(&output, resized_again, scale_only);
        assert_eq!(output.modes(), [scale_only.mode()]);
        assert_eq!(output.current_mode(), Some(scale_only.mode()));
        assert_eq!(output.preferred_mode(), Some(scale_only.mode()));
    }

    #[test]
    fn client_buffer_scale_must_be_positive() {
        assert_eq!(checked_buffer_scale(2).expect("valid scale"), 2);
        assert!(checked_buffer_scale(0).is_err());
        assert!(checked_buffer_scale(-1).is_err());
    }

    #[test]
    fn scale_only_surface_view_uses_the_full_buffer() {
        let view = translate_surface_content_view(
            metadata(1280, 960, 2),
            Size::<i32, Logical>::from((640, 480)),
            ViewportCachedState::default(),
        )
        .expect("valid scaled buffer");

        assert_eq!(
            view,
            SurfaceContentView {
                source_x: 0.0,
                source_y: 0.0,
                source_width: 1280.0,
                source_height: 960.0,
                logical_width: 640.0,
                logical_height: 480.0,
            }
        );
    }

    #[test]
    fn viewport_destination_defines_surface_logical_size() {
        let view = translate_surface_content_view(
            metadata(800, 600, 1),
            Size::<i32, Logical>::from((800, 600)),
            ViewportCachedState {
                src: None,
                dst: Some((640, 480).into()),
            },
        )
        .expect("valid fractional-scale viewport");

        assert_eq!(view.logical_width, 640.0);
        assert_eq!(view.logical_height, 480.0);
        assert_eq!(view.source_width, 800.0);
        assert_eq!(view.source_height, 600.0);
    }

    #[test]
    fn viewport_source_is_converted_from_logical_to_physical_pixels() {
        let view = translate_surface_content_view(
            metadata(1280, 960, 2),
            Size::<i32, Logical>::from((640, 480)),
            ViewportCachedState {
                src: Some(Rectangle::new((10.0, 20.0).into(), (100.0, 50.0).into())),
                dst: Some((200, 100).into()),
            },
        )
        .expect("valid cropped viewport");

        assert_eq!(view.source_x, 20.0);
        assert_eq!(view.source_y, 40.0);
        assert_eq!(view.source_width, 200.0);
        assert_eq!(view.source_height, 100.0);
        assert_eq!(view.logical_width, 200.0);
        assert_eq!(view.logical_height, 100.0);
    }

    #[test]
    fn rejects_out_of_bounds_viewport_sources() {
        let result = translate_surface_content_view(
            metadata(1280, 960, 2),
            Size::<i32, Logical>::from((640, 480)),
            ViewportCachedState {
                src: Some(Rectangle::new((600.0, 0.0).into(), (100.0, 50.0).into())),
                dst: Some((100, 50).into()),
            },
        );

        assert!(result.is_err());
    }

    #[test]
    fn rejects_non_normal_buffer_transforms() {
        let result = translate_surface_content_view(
            SurfaceBufferMetadata {
                transform: wl_output::Transform::_90,
                ..metadata(640, 480, 1)
            },
            Size::<i32, Logical>::from((480, 640)),
            ViewportCachedState::default(),
        );

        assert!(result.is_err());
    }

    #[test]
    fn strips_row_padding() {
        let source = [
            1, 2, 3, 4, 5, 6, 7, 8, 99, 99, 9, 10, 11, 12, 13, 14, 15, 16, 88, 88,
        ];
        let pixels = normalize_bgra_rows(&source, 2, 2, 10, wl_shm::Format::Argb8888)
            .expect("valid padded pixels");
        assert_eq!(
            pixels,
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn preserves_argb_alpha() {
        let pixels = normalize_bgra_rows(&[3, 2, 1, 17], 1, 1, 4, wl_shm::Format::Argb8888)
            .expect("valid ARGB pixel");
        assert_eq!(pixels, [3, 2, 1, 17]);
    }

    #[test]
    fn forces_xrgb_alpha_opaque() {
        let pixels = normalize_bgra_rows(&[3, 2, 1, 0], 1, 1, 4, wl_shm::Format::Xrgb8888)
            .expect("valid XRGB pixel");
        assert_eq!(pixels, [3, 2, 1, 255]);
    }

    #[test]
    fn rejects_short_rows() {
        assert!(normalize_bgra_rows(&[0; 7], 2, 1, 7, wl_shm::Format::Argb8888).is_err());
    }

    #[test]
    fn skips_empty_axis_frames() {
        assert!(
            smithay_axis_frame(
                RawScrollFrame {
                    source: RawScrollSource::Wheel,
                    horizontal: 0.0,
                    vertical: 0.0,
                    horizontal_v120: Some(0),
                    vertical_v120: None,
                },
                1,
            )
            .is_none()
        );
    }
}
