//! Nested Winit validation backend for Weld's Smithay, Bevy, and wgpu boundary.

use std::{
    collections::VecDeque,
    os::fd::{BorrowedFd, OwnedFd},
    sync::Arc,
    time::{Duration, Instant},
};

use crate::AppArguments;
use crate::raw_input::{
    InputPosition, LinuxButtonCode, LinuxKeycode, RawScrollFrame, RawScrollPhase, RawScrollSource,
    RawSeatEvent, RawSeatEventKind,
};
use crate::renderer::NestedRenderer;
use crate::runtime::{
    ChildProcesses, FrameState, HostCommand, LoopData, PendingCapture, iteration_work, server_mut,
};
#[cfg(test)]
use crate::runtime::{FRAME_INTERVAL, IterationWork};
use crate::server::{OutputDescriptor, OutputMetrics, ServerState};
use crate::shell::{ShellRenderer, ShellRendererOptions};
use anyhow::{Context, Result, anyhow, bail};
use bevy::math::UVec2;
use bevy_winit::converters::{convert_element_state, convert_logical_key};
use calloop::signals::Signals;
use smithay::reexports::{
    calloop::{EventLoop as CalloopEventLoop, Interest, Mode, PostAction, generic::Generic},
    wayland_server::Display,
};
use tracing::{info, trace, warn};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event::{MouseButton, MouseScrollDelta, TouchPhase, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle},
    platform::pump_events::{EventLoopExtPumpEvents, PumpStatus},
    platform::scancode::PhysicalKeyExtScancode,
    raw_window_handle::{HasDisplayHandle, RawDisplayHandle},
    window::{Window, WindowAttributes, WindowId},
};

enum NestedEvent {
    Command(HostCommand),
}

pub(crate) fn run(arguments: AppArguments, signals: Signals) -> Result<()> {
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
    let mut output_metrics = OutputMetrics::new(
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
        ShellRendererOptions {
            size: UVec2::new(initial_size.width, initial_size.height),
            scale_factor: initial_scale_factor,
            remote_debug: arguments.remote_debug.as_deref(),
            software_cursor: false,
        },
    )?;

    let mut calloop: CalloopEventLoop<'static, LoopData<NestedEvent>> =
        CalloopEventLoop::try_new().context("failed to create the Smithay calloop event loop")?;
    let display = Display::<ServerState>::new().context("failed to create the Wayland display")?;
    let server = ServerState::new(
        &calloop.handle(),
        display,
        started_at,
        "weld-seat0",
        OutputDescriptor::nested(),
        output_metrics,
        server_mut::<NestedEvent>,
    )?;
    let mut loop_data = LoopData::new(server);
    calloop
        .handle()
        .insert_source(signals, |event, _, data| {
            data.events
                .push_back(NestedEvent::Command(HostCommand::Exit));
            tracing::debug!(signal = ?event.signal(), "received shutdown signal");
        })
        .context("failed to register process signals")?;
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
    let mut children = ChildProcesses::default();
    let child_requested = children.spawn_requested(&loop_data.server, &arguments.client)?;
    let remote_debug_enabled = arguments.remote_debug.is_some();
    let mut pending_capture = arguments
        .screenshot
        .map(|path| PendingCapture::startup(path, child_requested));
    let mut frame_state = FrameState::default();
    let mut pending_presentation_id = None;

    info!(socket = ?loop_data.server.socket_name, "Weld nested compositor is ready");
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

        let input_pending = !host.input_events.is_empty();
        let host_work_drained = host_work_drained(
            input_pending,
            host.pending_size.is_some(),
            host.pending_scale_factor.is_some(),
        );
        if std::mem::take(&mut host.redraw_requested) {
            frame_state.request_present();
        }
        for event in host.input_events.drain(..) {
            shell.enqueue_input_event(event);
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
                OutputMetrics::new(physical_size.width, physical_size.height, scale_factor)?;
            loop_data.server.update_output_metrics(output_metrics);
            frame_state.request_composition();
        }

        calloop
            .dispatch(
                Some(dispatch_timeout(
                    host_work_drained,
                    &frame_state,
                    Instant::now(),
                )),
                &mut loop_data,
            )
            .context("Smithay calloop dispatch failed")?;
        for event in loop_data.server.take_surface_events() {
            shell.enqueue_surface_event(event);
            frame_state.request_composition();
        }
        if loop_data.server.presentation_requested() {
            frame_state.request_present();
        }

        let update_now = Instant::now();
        let work = iteration_work(
            input_pending,
            frame_state.composition_due(update_now),
            remote_debug_enabled,
            true,
        );
        let mut request_next_composition = false;
        let mut command_exit_requested = false;
        if work.advance_main {
            // `composition_advance` is intentionally the one shared predicate
            // for applying client surfaces and running RenderApp. Never
            // recompute the deadline between these two operations.
            let bevy_requested_redraw = shell.advance_main(
                started_at.elapsed().as_millis() as u32,
                work.composition_advance,
            );
            // ECS focus policy is authoritative and must be applied before the matching
            // pointer press establishes Smithay's implicit grab. Requests made during an
            // older grab are queued by the host and retried when that grab ends.
            for action in shell.take_surface_actions() {
                loop_data.server.apply_surface_action(action);
            }
            for effect in shell.take_input_effects() {
                loop_data.server.apply_input_effect(effect);
            }
            for command in shell.take_host_commands() {
                command_exit_requested |= children.apply(&loop_data.server, command)?;
            }
            if bevy_requested_redraw && !work.composition_advance {
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
            if command_exit_requested {
                break;
            }
            if work.composition_advance {
                shell.render_composition();
                pending_presentation_id = Some(loop_data.server.stage_surface_presentation());
                frame_state.composition_rendered(update_now);
                request_next_composition = bevy_requested_redraw;
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
        if frame_state.presentation_due() {
            let frame = renderer.render(shell.texture_view(), capture_path)?;
            if frame.presented {
                frame_state.presented();
                if let Some(presentation_id) = pending_presentation_id.take() {
                    loop_data.server.frame_presented(presentation_id);
                }
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
        // Keep this below presentation: a redraw requested while producing the
        // current frame schedules the next frame, but must not make the current
        // completed composition ineligible to present.
        if request_next_composition {
            frame_state.request_composition();
        }
        loop_data.server.flush_clients();
        let mut exit_requested = false;
        while let Some(event) = loop_data.events.pop_front() {
            match event {
                NestedEvent::Command(command) => {
                    exit_requested |= children.apply(&loop_data.server, command)?;
                }
            }
        }
        children.reap();
        if exit_requested {
            break;
        }
    }
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

fn dispatch_timeout(host_work_drained: bool, frame_state: &FrameState, now: Instant) -> Duration {
    if host_work_drained {
        Duration::ZERO
    } else {
        frame_state.composition_timeout(now, true)
    }
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
    active_scroll_axes: ActiveScrollAxes,
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
            active_scroll_axes: ActiveScrollAxes::default(),
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
                self.cancel_active_scroll(time);
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
                self.cancel_active_scroll(time);
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
            WindowEvent::MouseWheel { delta, phase, .. } => {
                let time = self.event_time();
                trace!(?delta, ?phase, "received nested host scroll");
                let axis = nested_axis(
                    delta,
                    phase,
                    self.scale_factor,
                    &mut self.active_scroll_axes,
                );
                self.input_events.push_back(RawSeatEvent::new(
                    RawSeatEventKind::PointerAxis {
                        position: self.pointer_position,
                        axis,
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

#[derive(Default)]
struct ActiveScrollAxes {
    horizontal: bool,
    vertical: bool,
}

impl NestedHost {
    fn cancel_active_scroll(&mut self, time: u32) {
        if !self.active_scroll_axes.horizontal && !self.active_scroll_axes.vertical {
            return;
        }
        self.input_events.push_back(RawSeatEvent::new(
            RawSeatEventKind::PointerAxis {
                position: self.pointer_position,
                axis: RawScrollFrame {
                    source: RawScrollSource::Finger,
                    phase: RawScrollPhase::Cancelled,
                    horizontal: 0.0,
                    vertical: 0.0,
                    horizontal_v120: None,
                    vertical_v120: None,
                    horizontal_stop: self.active_scroll_axes.horizontal,
                    vertical_stop: self.active_scroll_axes.vertical,
                },
            },
            time,
        ));
        self.active_scroll_axes = ActiveScrollAxes::default();
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

fn nested_axis(
    delta: MouseScrollDelta,
    phase: TouchPhase,
    scale_factor: f64,
    active: &mut ActiveScrollAxes,
) -> RawScrollFrame {
    match delta {
        MouseScrollDelta::LineDelta(horizontal, vertical) => RawScrollFrame {
            source: RawScrollSource::Wheel,
            phase: RawScrollPhase::Moved,
            horizontal: -f64::from(horizontal) * 15.0,
            vertical: -f64::from(vertical) * 15.0,
            horizontal_v120: Some((-horizontal * 120.0) as i32),
            vertical_v120: Some((-vertical * 120.0) as i32),
            horizontal_stop: false,
            vertical_stop: false,
        },
        MouseScrollDelta::PixelDelta(delta) => {
            let horizontal = -delta.x / scale_factor;
            let vertical = -delta.y / scale_factor;
            let phase = raw_scroll_phase(phase);
            let ending = matches!(phase, RawScrollPhase::Ended | RawScrollPhase::Cancelled);
            let horizontal_stop = ending && active.horizontal;
            let vertical_stop = ending && active.vertical;
            if ending {
                *active = ActiveScrollAxes::default();
            } else {
                active.horizontal |= horizontal != 0.0;
                active.vertical |= vertical != 0.0;
            }
            RawScrollFrame {
                source: RawScrollSource::Finger,
                phase,
                horizontal,
                vertical,
                horizontal_v120: None,
                vertical_v120: None,
                horizontal_stop,
                vertical_stop,
            }
        }
    }
}

const fn raw_scroll_phase(phase: TouchPhase) -> RawScrollPhase {
    match phase {
        TouchPhase::Started => RawScrollPhase::Started,
        TouchPhase::Moved => RawScrollPhase::Moved,
        TouchPhase::Ended => RawScrollPhase::Ended,
        TouchPhase::Cancelled => RawScrollPhase::Cancelled,
    }
}

fn logical_input_position(position: PhysicalPosition<f64>, scale_factor: f64) -> InputPosition {
    InputPosition::new(position.x / scale_factor, position.y / scale_factor)
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveScrollAxes, FRAME_INTERVAL, FrameState, IterationWork, dispatch_timeout,
        host_work_drained, iteration_work, linux_button_code, logical_input_position, nested_axis,
    };
    use crate::raw_input::{LinuxButtonCode, RawScrollFrame, RawScrollPhase, RawScrollSource};
    use std::time::Instant;
    use winit::{
        dpi::PhysicalPosition,
        event::{MouseButton, MouseScrollDelta},
    };

    #[test]
    fn input_or_resize_work_gets_one_nonblocking_smithay_dispatch() {
        let frame = FrameState::default();
        let now = Instant::now();
        assert_eq!(
            dispatch_timeout(host_work_drained(true, false, false), &frame, now),
            std::time::Duration::ZERO
        );
        assert_eq!(
            dispatch_timeout(host_work_drained(false, true, false), &frame, now),
            std::time::Duration::ZERO
        );
        assert_eq!(
            dispatch_timeout(host_work_drained(false, false, true), &frame, now),
            std::time::Duration::ZERO
        );
    }

    #[test]
    fn iterations_without_input_or_resize_keep_the_idle_timeout() {
        let now = Instant::now();
        let mut frame = FrameState::default();
        frame.composition_rendered(now);
        frame.presented();
        assert_eq!(
            dispatch_timeout(host_work_drained(false, false, false), &frame, now),
            FRAME_INTERVAL
        );
    }

    #[test]
    fn iteration_work_pairs_every_composition_with_one_main_advance() {
        assert_eq!(
            iteration_work(false, true, false, true),
            IterationWork {
                advance_main: true,
                composition_advance: true,
            }
        );
        assert_eq!(
            iteration_work(true, false, false, true),
            IterationWork {
                advance_main: true,
                composition_advance: false,
            }
        );
        assert_eq!(
            iteration_work(false, false, true, true),
            IterationWork {
                advance_main: true,
                composition_advance: true,
            }
        );
        assert_eq!(
            iteration_work(false, false, false, true),
            IterationWork {
                advance_main: false,
                composition_advance: false,
            }
        );
        assert_eq!(
            iteration_work(false, true, true, false),
            IterationWork {
                advance_main: false,
                composition_advance: false,
            }
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
            nested_axis(
                MouseScrollDelta::LineDelta(2.0, -3.0),
                winit::event::TouchPhase::Cancelled,
                1.25,
                &mut ActiveScrollAxes::default(),
            ),
            RawScrollFrame {
                source: RawScrollSource::Wheel,
                phase: RawScrollPhase::Moved,
                horizontal: -30.0,
                vertical: 45.0,
                horizontal_v120: Some(-240),
                vertical_v120: Some(360),
                horizontal_stop: false,
                vertical_stop: false,
            }
        );
        let mut active = ActiveScrollAxes::default();
        assert_eq!(
            nested_axis(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(5.0, -2.5)),
                winit::event::TouchPhase::Started,
                1.25,
                &mut active,
            ),
            RawScrollFrame {
                source: RawScrollSource::Finger,
                phase: RawScrollPhase::Started,
                horizontal: -4.0,
                vertical: 2.0,
                horizontal_v120: None,
                vertical_v120: None,
                horizontal_stop: false,
                vertical_stop: false,
            }
        );
        assert_eq!(
            nested_axis(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.0)),
                winit::event::TouchPhase::Ended,
                1.25,
                &mut active,
            ),
            RawScrollFrame {
                source: RawScrollSource::Finger,
                phase: RawScrollPhase::Ended,
                horizontal: 0.0,
                vertical: 0.0,
                horizontal_v120: None,
                vertical_v120: None,
                horizontal_stop: true,
                vertical_stop: true,
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
        let now = Instant::now();
        let mut frame = FrameState::default();
        frame.composition_rendered(now);
        frame.presented();

        frame.request_present();

        assert!(!frame.composition_dirty());
        assert!(frame.present_needed());
    }

    #[test]
    fn rendered_composition_remains_pending_until_presented() {
        let now = Instant::now();
        let mut frame = FrameState::default();

        frame.composition_rendered(now);

        assert!(!frame.composition_dirty());
        assert!(frame.present_needed());
        assert!(frame.presentation_due());
    }

    #[test]
    fn presentation_waits_for_the_dirty_composition() {
        let now = Instant::now();
        let mut frame = FrameState::default();

        assert!(frame.present_needed());
        assert!(!frame.presentation_due());

        frame.composition_rendered(now);

        assert!(frame.presentation_due());
    }

    #[test]
    fn next_frame_request_survives_presenting_the_current_composition() {
        let now = Instant::now();
        let mut frame = FrameState::default();
        frame.composition_rendered(now);

        assert!(frame.presentation_due());
        frame.presented();
        frame.request_composition();

        assert!(frame.composition_dirty());
        assert!(!frame.present_needed());
        assert!(!frame.presentation_due());
    }

    #[test]
    fn successful_presentation_clears_only_the_pending_present() {
        let now = Instant::now();
        let mut frame = FrameState::default();
        frame.composition_rendered(now);

        frame.presented();
        assert!(!frame.composition_dirty());
        assert!(!frame.present_needed());

        frame.request_composition();
        assert!(frame.composition_dirty());
        assert!(!frame.present_needed());
    }

    #[test]
    fn dirty_compositions_wait_for_the_next_frame_deadline() {
        let start = Instant::now();
        let mut frame = FrameState::default();
        frame.composition_rendered(start);
        frame.presented();
        frame.request_composition();
        let before_deadline = start + FRAME_INTERVAL / 2;

        assert!(!frame.composition_due(before_deadline));
        assert_eq!(
            dispatch_timeout(false, &frame, before_deadline),
            FRAME_INTERVAL / 2
        );
        assert!(frame.composition_due(start + FRAME_INTERVAL));
        assert_eq!(
            dispatch_timeout(false, &frame, start + FRAME_INTERVAL),
            std::time::Duration::ZERO
        );
    }
}
