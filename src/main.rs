//! Nested validation host for Weld's Smithay, Bevy, and wgpu boundary.

mod compositor;
mod debug;
mod input;
mod raw_input;
mod renderer;
mod server;
mod shell;

use std::{
    collections::VecDeque,
    ffi::OsString,
    os::fd::{BorrowedFd, OwnedFd},
    path::PathBuf,
    process::{Child, Command},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
// Cargo features cannot vary by profile. Referencing Bevy's dynamic-linking
// shim only when debug assertions are active makes plain `cargo run` use the
// fast development path while release binaries remain standalone.
use bevy::math::UVec2;
#[cfg(debug_assertions)]
use bevy_dylib as _;
use bevy_winit::converters::{convert_element_state, convert_logical_key};
use clap::Parser;
use raw_input::{
    InputPosition, LinuxButtonCode, LinuxKeycode, RawScrollFrame, RawScrollSource, RawSeatEvent,
    RawSeatEventKind,
};
use renderer::NestedRenderer;
use server::{NestedOutputMetrics, ServerState};
use shell::ShellRenderer;
use smithay::reexports::{
    calloop::{EventLoop as CalloopEventLoop, Interest, Mode, PostAction, generic::Generic},
    wayland_server::Display,
};
use tracing::{info, warn};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event::{MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle},
    platform::pump_events::{EventLoopExtPumpEvents, PumpStatus},
    platform::scancode::PhysicalKeyExtScancode,
    raw_window_handle::{HasDisplayHandle, RawDisplayHandle},
    window::{Window, WindowAttributes, WindowId},
};

const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
const CAPTURE_DEADLINE: Duration = Duration::from_secs(10);
const DEFAULT_REMOTE_ADDRESS: &str = "127.0.0.1:15702";

#[derive(Parser)]
#[command(
    version,
    about = "Nested compositor validation host",
    trailing_var_arg = true
)]
struct AppArguments {
    /// Enable the restricted Bevy Remote Protocol endpoint.
    #[arg(
        long,
        value_name = "HOST:PORT",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = DEFAULT_REMOTE_ADDRESS
    )]
    remote_debug: Option<String>,

    /// Capture the first settled composition and exit.
    #[arg(long, value_name = "PATH")]
    screenshot: Option<PathBuf>,

    /// Optional nested client program followed by its arguments.
    #[arg(value_name = "CLIENT_AND_ARGS", allow_hyphen_values = true)]
    client: Vec<OsString>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init()
        .map_err(|error| anyhow!("failed to initialize tracing: {error}"))?;
    let arguments = AppArguments::parse();
    if arguments
        .screenshot
        .as_ref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
        bail!("--screenshot requires a non-empty path");
    }
    let started_at = Instant::now();
    let mut host_event_loop =
        EventLoop::new().context("failed to create the nested host event loop")?;
    host_event_loop.set_control_flow(ControlFlow::Poll);
    let mut host = NestedHost::new(started_at);
    let initial_status = host_event_loop.pump_app_events(None, &mut host);
    if matches!(initial_status, PumpStatus::Exit(_)) {
        if arguments.screenshot.is_some() {
            bail!("nested host exited before the startup screenshot could be captured");
        }
        return Ok(());
    }
    let window = host
        .window
        .clone()
        .context("nested host did not create a window")?;
    if let Some(error) = host.creation_error.take() {
        bail!("failed to create the nested window: {error}");
    }

    let initial_size = nonzero_size(
        host.pending_size
            .take()
            .unwrap_or_else(|| window.inner_size()),
    );
    let initial_scale_factor = host
        .pending_scale_factor
        .take()
        .unwrap_or_else(|| window.scale_factor());
    let mut output_metrics = NestedOutputMetrics::new(
        initial_size.width,
        initial_size.height,
        initial_scale_factor,
    )?;
    let display_handle = host_event_loop.owned_display_handle();
    let host_wake_fd = host_display_wake_fd(&display_handle)?;
    let mut renderer = NestedRenderer::new(window, display_handle, initial_size)?;
    let mut shell = ShellRenderer::new(
        renderer.instance(),
        renderer.adapter(),
        renderer.device(),
        renderer.queue(),
        UVec2::new(initial_size.width, initial_size.height),
        initial_scale_factor,
        arguments.remote_debug.as_deref(),
    )?;

    let mut calloop: CalloopEventLoop<'static, ServerState> =
        CalloopEventLoop::try_new().context("failed to create the Smithay calloop event loop")?;
    let display = Display::<ServerState>::new().context("failed to create the Wayland display")?;
    let mut server = ServerState::new(&mut calloop, display, started_at, output_metrics)?;
    if let Some(host_wake_fd) = host_wake_fd {
        // Winit remains the sole reader. The duplicate descriptor only wakes
        // calloop so the next outer-loop iteration can pump host events.
        calloop
            .handle()
            .insert_source(
                // Edge mode prevents an unread backend-internal Wayland event
                // from turning the observer into a busy loop. The ordinary
                // frame timeout remains a bounded polling fallback.
                Generic::new(host_wake_fd, Interest::READ, Mode::Edge),
                |_, _, _| Ok(PostAction::Continue),
            )
            .context("failed to register the nested host display wake source")?;
    }
    let mut child = spawn_requested_client(&server, &arguments.client)?;
    let child_requested = !arguments.client.is_empty();
    let remote_debug_enabled = arguments.remote_debug.is_some();
    let mut pending_capture = arguments
        .screenshot
        .map(|path| PendingCapture::startup(path, child_requested));
    let mut frame_state = FrameState::default();

    info!(socket = ?server.socket_name, "Weld nested compositor is ready");
    loop {
        let host_exited = matches!(
            host_event_loop.pump_app_events(Some(Duration::ZERO), &mut host),
            PumpStatus::Exit(_)
        ) || host.close_requested;
        host.refresh_scale_factor();
        if host_exited {
            if pending_capture
                .as_ref()
                .is_some_and(PendingCapture::is_startup)
            {
                bail!("nested host closed before the startup screenshot completed");
            }
            break;
        }

        let host_work_drained = host_work_drained(
            !host.input_events.is_empty(),
            host.pending_size.is_some(),
            host.pending_scale_factor.is_some(),
        );
        if std::mem::take(&mut host.redraw_requested) {
            frame_state.request_present();
        }
        for event in host.input_events.drain(..) {
            shell.enqueue_input_event(event);
            // Keep extraction paired with the main-world input update. A future
            // input-only schedule may remove this speculative composition after
            // its rendering-state invariants are proven.
            frame_state.request_composition();
        }
        let pending_size = host.pending_size.take();
        let pending_scale_factor = host.pending_scale_factor.take();
        let mut metrics_changed = false;
        let mut physical_size = PhysicalSize::new(
            output_metrics.physical_width(),
            output_metrics.physical_height(),
        );
        let mut scale_factor = output_metrics.scale_factor();
        if let Some(size) = pending_size
            && size.width > 0
            && size.height > 0
            && size != physical_size
        {
            physical_size = size;
            renderer.resize(size);
            shell.resize(size.width, size.height);
            metrics_changed = true;
        }
        if let Some(pending_scale_factor) = pending_scale_factor
            && pending_scale_factor != scale_factor
        {
            scale_factor = pending_scale_factor;
            shell.set_scale_factor(scale_factor);
            metrics_changed = true;
        }
        if metrics_changed {
            output_metrics =
                NestedOutputMetrics::new(physical_size.width, physical_size.height, scale_factor)?;
            server.update_output_metrics(output_metrics);
            frame_state.request_composition();
        }

        calloop
            .dispatch(Some(dispatch_timeout(host_work_drained)), &mut server)
            .context("Smithay calloop dispatch failed")?;
        for event in server.take_surface_events() {
            shell.enqueue_surface_event(event);
            frame_state.request_composition();
        }
        if server.presentation_requested() {
            frame_state.request_present();
        }

        if frame_state.composition_dirty() || remote_debug_enabled {
            // Remote debugging remains a full paired Bevy update so extraction cannot miss
            // main-world changes. Ordinary runs advance for host-driven visual changes or an
            // explicit Bevy RequestRedraw from the preceding update.
            shell.update(started_at.elapsed().as_millis() as u32);
            let bevy_requested_redraw = shell.take_redraw_request();
            for effect in shell.take_input_effects() {
                server.apply_input_effect(effect);
            }
            frame_state.composition_rendered();
            if bevy_requested_redraw {
                frame_state.request_composition();
            }
            if shell.should_exit() {
                if pending_capture
                    .as_ref()
                    .is_some_and(PendingCapture::is_startup)
                {
                    bail!("Bevy exited before the startup screenshot completed");
                }
                break;
            }
        }

        if pending_capture.is_none()
            && let Some(request) = shell.take_capture_request()
        {
            pending_capture = Some(PendingCapture::remote(request.request_id, request.path));
            frame_state.request_present();
        }
        if pending_capture
            .as_ref()
            .is_some_and(|capture| capture.deadline <= Instant::now())
            && let Some(capture) = pending_capture.take()
        {
            let error = "screenshot timed out before a presentable frame was available".to_owned();
            if let Some(request_id) = capture.remote_request_id {
                shell.complete_capture(request_id, Err(error));
            } else {
                bail!("startup {error}");
            }
        }

        let capture_path = pending_capture.as_ref().and_then(|capture| {
            let client_ready = !capture.wait_for_client || shell.has_surface_frame();
            client_ready.then_some(capture.path.as_path())
        });
        if capture_path.is_some() {
            frame_state.request_present();
        }
        if frame_state.present_needed() {
            let frame = renderer.render(shell.texture_view(), capture_path)?;
            if frame.presented {
                frame_state.presented();
                server.frame_presented();
            }
            if let Some(capture_result) = frame.capture
                && let Some(capture) = pending_capture.take()
            {
                match capture.remote_request_id {
                    Some(request_id) => {
                        if let Err(error) = &capture_result {
                            warn!(request_id, %error, "remote screenshot failed");
                        }
                        shell.complete_capture(request_id, capture_result);
                    }
                    None => match capture_result {
                        Ok(()) => {
                            info!(path = %capture.path.display(), "startup screenshot saved");
                            return Ok(());
                        }
                        Err(error) => bail!("startup screenshot failed: {error}"),
                    },
                }
            }
        }
        server.flush_clients();

        if child
            .as_mut()
            .is_some_and(|process| process.try_wait().ok().flatten().is_some())
        {
            child = None;
        }
    }

    drop(child);
    Ok(())
}

fn host_display_wake_fd(display: &OwnedDisplayHandle) -> Result<Option<OwnedFd>> {
    let raw_display = display
        .display_handle()
        .context("nested host did not expose a raw display handle")?
        .as_raw();
    let raw_fd = match raw_display {
        RawDisplayHandle::Wayland(handle) => {
            // SAFETY: Winit owns this live wl_display for at least as long as
            // `display`. The function only queries its connection descriptor.
            unsafe {
                (wayland_sys::client::wayland_client_handle().wl_display_get_fd)(
                    handle.display.as_ptr().cast(),
                )
            }
        }
        RawDisplayHandle::Xlib(handle) => {
            let Some(display) = handle.display else {
                warn!("Xlib host display has no connection pointer; using timer polling");
                return Ok(None);
            };
            let xlib = x11_dl::xlib::Xlib::open()
                .map_err(|error| anyhow!("failed to load Xlib for its connection fd: {error}"))?;
            // Winit's X11 backend retains its own libX11 reference for the
            // display lifetime, so this temporary lookup handle may drop.
            // SAFETY: The raw display pointer belongs to Winit and is live for
            // the duration of this call. XConnectionNumber does not take ownership.
            unsafe { (xlib.XConnectionNumber)(display.as_ptr().cast()) }
        }
        _ => {
            warn!(
                ?raw_display,
                "host display cannot wake calloop; using timer polling"
            );
            return Ok(None);
        }
    };
    if raw_fd < 0 {
        warn!(
            raw_fd,
            "host display returned an invalid connection fd; using timer polling"
        );
        return Ok(None);
    }
    // SAFETY: The descriptor is owned by Winit and valid at this point. It is
    // borrowed only long enough to duplicate it into an independently owned fd.
    let borrowed = unsafe { BorrowedFd::borrow_raw(raw_fd) };
    borrowed
        .try_clone_to_owned()
        .map(Some)
        .context("failed to duplicate the nested host display connection fd")
}

const fn host_work_drained(input_pending: bool, resize_pending: bool, scale_pending: bool) -> bool {
    // A minimized 0x0 resize still counts: pending_size is consumed even when
    // the renderer resize is skipped. RedrawRequested only asks for a present
    // and deliberately retains the ordinary bounded dispatch timeout.
    input_pending || resize_pending || scale_pending
}

const fn dispatch_timeout(host_work_drained: bool) -> Duration {
    if host_work_drained {
        Duration::ZERO
    } else {
        FRAME_INTERVAL
    }
}

#[derive(Debug)]
struct FrameState {
    composition_dirty: bool,
    present_needed: bool,
}

impl Default for FrameState {
    fn default() -> Self {
        Self {
            composition_dirty: true,
            present_needed: true,
        }
    }
}

impl FrameState {
    const fn composition_dirty(&self) -> bool {
        self.composition_dirty
    }

    const fn present_needed(&self) -> bool {
        self.present_needed
    }

    fn request_composition(&mut self) {
        self.composition_dirty = true;
        self.present_needed = true;
    }

    fn request_present(&mut self) {
        self.present_needed = true;
    }

    fn composition_rendered(&mut self) {
        self.composition_dirty = false;
        self.present_needed = true;
    }

    fn presented(&mut self) {
        self.present_needed = false;
    }
}

struct PendingCapture {
    path: PathBuf,
    remote_request_id: Option<u64>,
    deadline: Instant,
    wait_for_client: bool,
}

impl PendingCapture {
    fn startup(path: PathBuf, wait_for_client: bool) -> Self {
        Self {
            path,
            remote_request_id: None,
            deadline: Instant::now() + CAPTURE_DEADLINE,
            wait_for_client,
        }
    }

    fn remote(request_id: u64, path: PathBuf) -> Self {
        Self {
            path,
            remote_request_id: Some(request_id),
            deadline: Instant::now() + CAPTURE_DEADLINE,
            wait_for_client: false,
        }
    }

    const fn is_startup(&self) -> bool {
        self.remote_request_id.is_none()
    }
}

fn spawn_requested_client(server: &ServerState, arguments: &[OsString]) -> Result<Option<Child>> {
    let Some((program, arguments)) = arguments.split_first() else {
        return Ok(None);
    };
    let child = Command::new(program)
        .args(arguments)
        .env("WAYLAND_DISPLAY", &server.socket_name)
        .spawn()
        .with_context(|| format!("failed to spawn nested client {program:?}"))?;
    Ok(Some(child))
}

fn nonzero_size(size: PhysicalSize<u32>) -> PhysicalSize<u32> {
    PhysicalSize::new(size.width.max(1), size.height.max(1))
}

struct NestedHost {
    window: Option<Arc<Window>>,
    pending_size: Option<PhysicalSize<u32>>,
    pending_scale_factor: Option<f64>,
    input_events: VecDeque<RawSeatEvent>,
    pointer_position: Option<InputPosition>,
    scale_factor: f64,
    redraw_requested: bool,
    creation_error: Option<String>,
    close_requested: bool,
    started_at: Instant,
}

impl NestedHost {
    fn new(started_at: Instant) -> Self {
        Self {
            window: None,
            pending_size: None,
            pending_scale_factor: None,
            input_events: VecDeque::new(),
            pointer_position: None,
            scale_factor: 1.0,
            redraw_requested: false,
            creation_error: None,
            close_requested: false,
            started_at,
        }
    }

    fn event_time(&self) -> u32 {
        self.started_at.elapsed().as_millis() as u32
    }

    fn refresh_scale_factor(&mut self) {
        let Some(window) = &self.window else {
            return;
        };
        let scale_factor = window.scale_factor();
        if scale_factor != self.scale_factor {
            self.scale_factor = scale_factor;
            self.pending_scale_factor = Some(scale_factor);
        }
    }
}

impl ApplicationHandler for NestedHost {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.creation_error.is_some() {
            return;
        }
        let attributes = WindowAttributes::default()
            .with_title("Weld nested compositor")
            .with_inner_size(LogicalSize::new(960.0, 640.0));
        match event_loop.create_window(attributes) {
            Ok(window) => {
                let window = Arc::new(window);
                self.pending_size = Some(window.inner_size());
                self.scale_factor = window.scale_factor();
                self.pending_scale_factor = Some(self.scale_factor);
                self.window = Some(window);
            }
            Err(error) => self.creation_error = Some(error.to_string()),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                self.close_requested = true;
                event_loop.exit();
            }
            WindowEvent::Resized(size) => self.pending_size = Some(size),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor;
                self.pending_scale_factor = Some(scale_factor);
            }
            WindowEvent::RedrawRequested => self.redraw_requested = true,
            WindowEvent::Focused(false) => {
                self.pointer_position = None;
                let time = self.event_time();
                self.input_events
                    .push_back(RawSeatEvent::new(RawSeatEventKind::HostFocusLost, time));
            }
            // Click-to-focus is deliberate in this initial slice: regaining host
            // focus does not restore the previously focused client automatically.
            WindowEvent::Focused(true) => {}
            WindowEvent::CursorMoved { position, .. } => {
                let position = logical_input_position(position, self.scale_factor);
                self.pointer_position = Some(position);
                let time = self.event_time();
                self.input_events.push_back(RawSeatEvent::new(
                    RawSeatEventKind::PointerMotion { position },
                    time,
                ));
            }
            WindowEvent::CursorLeft { .. } => {
                let position = self.pointer_position.take().unwrap_or_default();
                let time = self.event_time();
                self.input_events.push_back(RawSeatEvent::new(
                    RawSeatEventKind::PointerLeft { position },
                    time,
                ));
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = linux_button_code(button) {
                    let time = self.event_time();
                    self.input_events.push_back(RawSeatEvent::new(
                        RawSeatEventKind::PointerButton {
                            position: self.pointer_position,
                            button,
                            state: convert_element_state(state),
                        },
                        time,
                    ));
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let time = self.event_time();
                self.input_events.push_back(RawSeatEvent::new(
                    RawSeatEventKind::PointerAxis {
                        position: self.pointer_position,
                        axis: nested_axis(delta, self.scale_factor),
                    },
                    time,
                ));
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } if !is_synthetic && !event.repeat => {
                if let Some(keycode) = event.physical_key.to_scancode() {
                    let time = self.event_time();
                    self.input_events.push_back(RawSeatEvent::new(
                        RawSeatEventKind::Keyboard {
                            keycode: LinuxKeycode(keycode),
                            logical_key: Some(convert_logical_key(&event.logical_key)),
                            state: convert_element_state(event.state),
                        },
                        time,
                    ));
                }
            }
            _ => {}
        }
    }
}

const fn linux_button_code(button: MouseButton) -> Option<LinuxButtonCode> {
    match button {
        MouseButton::Left => Some(LinuxButtonCode(0x110)),
        MouseButton::Right => Some(LinuxButtonCode(0x111)),
        MouseButton::Middle => Some(LinuxButtonCode(0x112)),
        MouseButton::Forward => Some(LinuxButtonCode(0x114)),
        MouseButton::Back => Some(LinuxButtonCode(0x113)),
        MouseButton::Other(_) => None,
    }
}

fn nested_axis(delta: MouseScrollDelta, scale_factor: f64) -> RawScrollFrame {
    match delta {
        MouseScrollDelta::LineDelta(horizontal, vertical) => RawScrollFrame {
            source: RawScrollSource::Wheel,
            horizontal: -f64::from(horizontal) * 15.0,
            vertical: -f64::from(vertical) * 15.0,
            horizontal_v120: Some((-horizontal * 120.0) as i32),
            vertical_v120: Some((-vertical * 120.0) as i32),
        },
        MouseScrollDelta::PixelDelta(delta) => RawScrollFrame {
            source: RawScrollSource::Continuous,
            horizontal: -delta.x / scale_factor,
            vertical: -delta.y / scale_factor,
            horizontal_v120: None,
            vertical_v120: None,
        },
    }
}

fn logical_input_position(position: PhysicalPosition<f64>, scale_factor: f64) -> InputPosition {
    InputPosition::new(position.x / scale_factor, position.y / scale_factor)
}

#[cfg(test)]
mod tests {
    use super::{
        FRAME_INTERVAL, FrameState, dispatch_timeout, host_work_drained, linux_button_code,
        logical_input_position, nested_axis,
    };
    use crate::raw_input::{LinuxButtonCode, RawScrollFrame, RawScrollSource};
    use winit::{
        dpi::PhysicalPosition,
        event::{MouseButton, MouseScrollDelta},
    };

    #[test]
    fn input_or_resize_work_gets_one_nonblocking_smithay_dispatch() {
        assert_eq!(
            dispatch_timeout(host_work_drained(true, false, false)),
            std::time::Duration::ZERO
        );
        assert_eq!(
            dispatch_timeout(host_work_drained(false, true, false)),
            std::time::Duration::ZERO
        );
        assert_eq!(
            dispatch_timeout(host_work_drained(false, false, true)),
            std::time::Duration::ZERO
        );
    }

    #[test]
    fn iterations_without_input_or_resize_keep_the_idle_timeout() {
        assert_eq!(
            dispatch_timeout(host_work_drained(false, false, false)),
            FRAME_INTERVAL
        );
    }

    #[test]
    fn mouse_buttons_map_to_canonical_linux_codes() {
        let cases = [
            (MouseButton::Left, Some(LinuxButtonCode(0x110))),
            (MouseButton::Right, Some(LinuxButtonCode(0x111))),
            (MouseButton::Middle, Some(LinuxButtonCode(0x112))),
            (MouseButton::Back, Some(LinuxButtonCode(0x113))),
            (MouseButton::Forward, Some(LinuxButtonCode(0x114))),
            (MouseButton::Other(9), None),
        ];

        for (button, expected) in cases {
            assert_eq!(linux_button_code(button), expected);
        }
    }

    #[test]
    fn nested_scroll_converts_to_wayland_axis_direction() {
        assert_eq!(
            nested_axis(MouseScrollDelta::LineDelta(2.0, -3.0), 1.25),
            RawScrollFrame {
                source: RawScrollSource::Wheel,
                horizontal: -30.0,
                vertical: 45.0,
                horizontal_v120: Some(-240),
                vertical_v120: Some(360),
            }
        );
        assert_eq!(
            nested_axis(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(5.0, -2.5)),
                1.25,
            ),
            RawScrollFrame {
                source: RawScrollSource::Continuous,
                horizontal: -4.0,
                vertical: 2.0,
                horizontal_v120: None,
                vertical_v120: None,
            }
        );
    }

    #[test]
    fn host_physical_pointer_positions_become_compositor_logical() {
        assert_eq!(
            logical_input_position(PhysicalPosition::new(100.0, 50.0), 1.25),
            crate::raw_input::InputPosition::new(80.0, 40.0)
        );
    }

    #[test]
    fn initial_frame_requires_composition_and_presentation() {
        let frame = FrameState::default();

        assert!(frame.composition_dirty());
        assert!(frame.present_needed());
    }

    #[test]
    fn host_redraw_reuses_the_cached_composition() {
        let mut frame = FrameState::default();
        frame.composition_rendered();
        frame.presented();

        frame.request_present();

        assert!(!frame.composition_dirty());
        assert!(frame.present_needed());
    }

    #[test]
    fn rendered_composition_remains_pending_until_presented() {
        let mut frame = FrameState::default();

        frame.composition_rendered();

        assert!(!frame.composition_dirty());
        assert!(frame.present_needed());
    }

    #[test]
    fn next_frame_request_survives_presenting_the_current_composition() {
        let mut frame = FrameState::default();
        frame.composition_rendered();
        frame.request_composition();

        frame.presented();

        assert!(frame.composition_dirty());
        assert!(!frame.present_needed());
    }

    #[test]
    fn successful_presentation_clears_only_the_pending_present() {
        let mut frame = FrameState::default();
        frame.composition_rendered();

        frame.presented();
        assert!(!frame.composition_dirty());
        assert!(!frame.present_needed());

        frame.request_composition();
        assert!(frame.composition_dirty());
        assert!(frame.present_needed());
    }
}
