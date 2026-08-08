//! Minimal Smithay host for the nested SHM rendering experiment.

use std::{ffi::OsString, sync::Arc, time::Instant};

use anyhow::{Context, Result, anyhow, bail};
use smithay::{
    input::{Seat, SeatHandler, SeatState, dnd::DndGrabHandler, pointer::CursorImageStatus},
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic},
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer, wl_seat, wl_shm, wl_surface::WlSurface},
        },
    },
    utils::{Logical, Serial, Size, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, with_states,
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
    },
};
use tracing::{debug, info, warn};

use crate::compositor::{HostSurfaceEvent, SurfaceEventQueue, SurfaceFrame, SurfaceId};

const CLIENT_WIDTH: i32 = 640;
const CLIENT_HEIGHT: i32 = 480;
const OUTPUT_WIDTH: i32 = 960;
const OUTPUT_HEIGHT: i32 = 640;

struct ActiveToplevel {
    surface: ToplevelSurface,
    id: SurfaceId,
}

/// Smithay state kept outside Bevy's ECS world.
pub struct ServerState {
    pub display_handle: DisplayHandle,
    pub socket_name: OsString,
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    _output_manager_state: OutputManagerState,
    seat_state: SeatState<Self>,
    data_device_state: DataDeviceState,
    _seat: Seat<Self>,
    output: Output,
    active_toplevel: Option<ActiveToplevel>,
    pending_surface_events: SurfaceEventQueue,
    presentation_requested: bool,
    next_surface_id: u64,
    started_at: Instant,
}

impl ServerState {
    pub fn new(event_loop: &mut EventLoop<'static, Self>, display: Display<Self>) -> Result<Self> {
        let display_handle = display.handle();
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, []);
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
        let output_mode = OutputMode {
            size: (OUTPUT_WIDTH, OUTPUT_HEIGHT).into(),
            refresh: 60_000,
        };
        output.create_global::<Self>(&display_handle);
        output.change_current_state(
            Some(output_mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        output.set_preferred(output_mode);

        let listening_socket = ListeningSocketSource::new_auto()
            .context("failed to create a Wayland listening socket")?;
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
            _output_manager_state: output_manager_state,
            seat_state,
            data_device_state,
            _seat: seat,
            output,
            active_toplevel: None,
            pending_surface_events: SurfaceEventQueue::default(),
            presentation_requested: false,
            next_surface_id: 1,
            started_at: Instant::now(),
        })
    }

    pub fn take_surface_events(&mut self) -> impl Iterator<Item = HostSurfaceEvent> + '_ {
        self.pending_surface_events.drain()
    }

    pub const fn presentation_requested(&self) -> bool {
        self.presentation_requested
    }

    pub fn frame_presented(&mut self) {
        // No protocol dispatch occurs between observing this request and acknowledging a
        // successful present, so clearing it here cannot discard a newer client commit.
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

    fn handle_root_commit(&mut self, surface: &WlSurface) {
        let Some(surface_id) = self.active_toplevel.as_ref().map(|toplevel| toplevel.id) else {
            return;
        };
        let assignment = with_states(surface, |states| {
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            attributes.current().buffer.take()
        });

        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let copied = copy_shm_buffer(&buffer);
                buffer.release();
                match copied {
                    Ok(frame) => self.pending_surface_events.push(HostSurfaceEvent::Frame {
                        surface: surface_id,
                        frame,
                    }),
                    Err(error) => warn!(%error, "ignored an unsupported client buffer"),
                }
            }
            Some(BufferAssignment::Removed) => {
                self.pending_surface_events
                    .push(HostSurfaceEvent::Unmapped {
                        surface: surface_id,
                    });
            }
            None => {}
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

impl BufferHandler for ServerState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for ServerState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
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
        surface.send_configure();
        let id = self.allocate_surface_id();
        self.pending_surface_events
            .push(HostSurfaceEvent::Mapped { surface: id });
        info!(surface_id = id.raw(), "mapped a nested xdg-toplevel");
        self.active_toplevel = Some(ActiveToplevel { surface, id });
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

fn copy_shm_buffer(buffer: &wl_buffer::WlBuffer) -> Result<SurfaceFrame> {
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
) -> Result<SurfaceFrame> {
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

    Ok(SurfaceFrame {
        width: u32::try_from(width).context("SHM width exceeds u32")?,
        height: u32::try_from(height).context("SHM height exceeds u32")?,
        bgra_pixels: pixels,
        opaque: data.format == wl_shm::Format::Xrgb8888,
    })
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
    use super::normalize_bgra_rows;
    use smithay::reexports::wayland_server::protocol::wl_shm;

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
}
