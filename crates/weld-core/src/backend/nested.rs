//! Nested Winit validation backend for Weld's Smithay, Bevy, and wgpu boundary.

use std::{
    os::fd::{BorrowedFd, OwnedFd},
    sync::Arc,
    time::{Duration, Instant},
};

use crate::input::source::nested::NestedAdapter;
use crate::renderer::NestedRenderer;
#[cfg(test)]
use crate::runtime::{BEVY_SETTLE_COMPOSITIONS, FRAME_INTERVAL, IterationWork};
use crate::runtime::{
    ChildProcesses, FrameState, HostCommand, HostCommandEffect, LoopData, PendingCapture,
    iteration_work, server_mut,
};
use crate::server::{
    OutputDescriptor, OutputMetrics, ServerOptions, ServerOutputDefinition, ServerState,
};
use crate::{
    OutputConfiguration, OutputHead, OutputId, OutputScale,
    cursor::CursorImage,
    host::{
        CompositionDemand, CompositionDestination, CompositionFrame, CompositionOutputRequest,
        PreparedHost, RenderContext, RunOptions,
    },
    surface::LogicalPoint,
};
use anyhow::{Context, Result, anyhow, bail};
use calloop::channel;
use calloop::signals::Signals;
use smithay::reexports::{
    calloop::{EventLoop as CalloopEventLoop, Interest, Mode, PostAction, generic::Generic},
    wayland_server::Display,
};
use tracing::{info, warn};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalSize},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle},
    platform::pump_events::{EventLoopExtPumpEvents, PumpStatus},
    raw_window_handle::{HasDisplayHandle, RawDisplayHandle},
    window::{Window, WindowAttributes, WindowId},
};

enum NestedEvent {
    Command(HostCommand),
}

pub(crate) fn prepare(options: RunOptions, signals: Signals) -> Result<PreparedHost> {
    let _prepare_span =
        tracing::trace_span!(target: crate::PROFILE_TARGET, "nested_backend_prepare").entered();

    let started_at = Instant::now();
    let mut host_event_loop =
        EventLoop::new().context("failed to create the nested host event loop")?;
    host_event_loop.set_control_flow(ControlFlow::Poll);
    let mut host = NestedHost::new(started_at);
    let initial_status = host_event_loop.pump_app_events(None, &mut host);
    if matches!(initial_status, PumpStatus::Exit(_)) {
        if options.screenshot.is_some() {
            bail!("nested host exited before the startup screenshot could be captured");
        }
        bail!("nested host exited during preparation");
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
        OutputScale::new(initial_scale_factor)?,
    )?;
    let display_handle = host_event_loop.owned_display_handle();
    let host_wake_fd = host_display_wake_fd(&display_handle)?;
    let mut renderer = NestedRenderer::new(window, display_handle, initial_size)?;
    let (dmabuf_release_sender, dmabuf_release_source) = channel::channel();
    let initial_extent = crate::surface::Extent::new(initial_size.width, initial_size.height);
    let nested_output = OutputConfiguration::new(
        OutputId::new(1),
        initial_extent,
        OutputScale::new(initial_scale_factor)?,
        LogicalPoint::ZERO,
        true,
        None,
    )?;
    let context = RenderContext {
        instance: renderer.instance().clone(),
        adapter: renderer.adapter().clone(),
        device: renderer.device().clone(),
        queue: renderer.queue().clone(),
        dmabuf: crate::dmabuf::DmabufContext::new(dmabuf_release_sender, renderer.dmabuf_sources()),
        output_heads: vec![OutputHead::new(OutputId::new(1), "weld-nested", None)],
        outputs: vec![nested_output],
        composition_format: wgpu::TextureFormat::Rgba8UnormSrgb,
    };

    Ok(PreparedHost::new(context, move |application| {
        let mut shell = application;

        let mut calloop: CalloopEventLoop<'static, LoopData<NestedEvent>> =
            CalloopEventLoop::try_new()
                .context("failed to create the Smithay calloop event loop")?;
        let display =
            Display::<ServerState>::new().context("failed to create the Wayland display")?;
        let server = ServerState::new(
            &calloop.handle(),
            display,
            dmabuf_release_source,
            server_mut::<NestedEvent, ()>,
            ServerOptions {
                started_at,
                seat_name: "weld-seat0",
                outputs: vec![ServerOutputDefinition {
                    id: OutputId::new(1),
                    descriptor: OutputDescriptor::nested(),
                    metrics: output_metrics,
                    logical_position: (0, 0),
                    primary: true,
                }],
                dmabuf_capabilities: renderer.dmabuf_capabilities(),
                dmabuf_sources: renderer.dmabuf_sources(),
            },
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
        let child_requested = children.spawn_requested(&loop_data.server, &options.client)?;
        let remote_debug_enabled = options.remote_debug_enabled;
        let mut pending_capture = options
            .screenshot
            .map(|path| PendingCapture::startup(path, child_requested));
        let mut frame_state = FrameState::default();
        let mut completed_composition: Option<CompositionFrame> = None;
        let mut next_remote_service = Instant::now();
        let mut pending_presentation_id = None;

        info!(socket = ?loop_data.server.socket_name, "Weld nested compositor is ready");
        loop {
            let pump_status = {
                let _pump_span =
                    tracing::trace_span!(target: crate::PROFILE_TARGET, "winit_pump_events")
                        .entered();
                host_event_loop.pump_app_events(Some(Duration::ZERO), &mut host)
            };
            let host_exited = matches!(pump_status, PumpStatus::Exit(_)) || host.close_requested;
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

            let input_pending = host.input.has_pending();
            let host_work_drained = host_work_drained(
                input_pending,
                host.pending_size.is_some(),
                host.pending_scale_factor.is_some(),
            );
            if std::mem::take(&mut host.redraw_requested) {
                frame_state.request_present();
            }
            if input_pending {
                frame_state.request_update();
                let _input_span = tracing::trace_span!(
                    target: crate::PROFILE_TARGET,
                    "nested_host_input_ingress"
                )
                .entered();
                let mut event_count = 0_usize;
                for event in host.input.drain() {
                    if shell.enqueue_input_event(event.clone()) {
                        loop_data.server.forward_raw_input(event);
                    }
                    event_count += 1;
                }
                tracing::trace!(
                    target: crate::PROFILE_TARGET,
                    event_count,
                    "host input batch"
                );
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
                metrics_changed = true;
            }
            if let Some(pending_scale_factor) = pending_scale_factor
                && pending_scale_factor != scale_factor
            {
                scale_factor = pending_scale_factor;
                metrics_changed = true;
            }
            if metrics_changed {
                let candidate = OutputScale::new(scale_factor).and_then(|scale| {
                    OutputMetrics::new(physical_size.width, physical_size.height, scale)
                });
                match candidate {
                    Ok(candidate) => {
                        if physical_size.width != output_metrics.physical_width()
                            || physical_size.height != output_metrics.physical_height()
                        {
                            renderer.resize(physical_size);
                        }
                        output_metrics = candidate;
                        loop_data.server.update_output_metrics(output_metrics);
                        let configuration = OutputConfiguration::new(
                            OutputId::new(1),
                            crate::surface::Extent::new(physical_size.width, physical_size.height),
                            OutputScale::new(scale_factor)?,
                            LogicalPoint::ZERO,
                            true,
                            None,
                        )?;
                        shell.update_output_topology(&[configuration]);
                        frame_state.request_composition();
                    }
                    Err(error) => warn!(
                        width = physical_size.width,
                        height = physical_size.height,
                        scale_factor,
                        %error,
                        "ignored invalid nested output geometry"
                    ),
                }
            }

            let timeout = dispatch_timeout(host_work_drained, &frame_state, Instant::now());
            {
                let _dispatch_span = tracing::trace_span!(
                    target: crate::PROFILE_TARGET,
                    "nested_calloop_wait_and_dispatch"
                )
                .entered();
                calloop
                    .dispatch(Some(timeout), &mut loop_data)
                    .context("Smithay calloop dispatch failed")?;
            }
            if loop_data.server.has_surface_events() {
                let _surface_span = tracing::trace_span!(
                    target: crate::PROFILE_TARGET,
                    "nested_host_surface_ingress"
                )
                .entered();
                let mut event_count = 0_usize;
                for event in loop_data.server.take_surface_events() {
                    match shell.enqueue_surface_event(event) {
                        CompositionDemand::Ordinary => frame_state.request_composition(),
                        CompositionDemand::Settle => frame_state.request_settled_composition(),
                    }
                    event_count += 1;
                }
                tracing::trace!(
                    target: crate::PROFILE_TARGET,
                    event_count,
                    "host surface batch"
                );
            }
            if loop_data.server.presentation_requested() {
                frame_state.request_present();
            }

            let update_now = Instant::now();
            if remote_debug_enabled && update_now >= next_remote_service {
                shell.service_remote_debug();
                next_remote_service =
                    update_now + crate::runtime::REMOTE_DEBUG_MAINTENANCE_INTERVAL;
            }

            let mut work = iteration_work(
                frame_state.update_due(update_now),
                frame_state.composition_due(update_now),
            );
            let mut request_next_composition = false;
            let mut command_exit_requested = false;
            if work.advance_main {
                let bevy_requested_redraw =
                    shell.advance_main(started_at.elapsed().as_millis() as u32);
                if bevy_requested_redraw {
                    frame_state.request_composition();
                    work.render_composition = true;
                }
                let surface_actions = shell.take_surface_actions();
                let input_effects = shell.take_input_effects();
                let host_commands = shell.take_host_commands();
                let cursor_update = shell.take_cursor_update();
                if !surface_actions.is_empty()
                    || !input_effects.is_empty()
                    || !host_commands.is_empty()
                {
                    let _results_span = tracing::trace_span!(
                        target: crate::PROFILE_TARGET,
                        "nested_apply_ecs_results"
                    )
                    .entered();
                    tracing::trace!(
                        target: crate::PROFILE_TARGET,
                        surface_actions = surface_actions.len(),
                        input_effects = input_effects.len(),
                        host_commands = host_commands.len(),
                        "ECS result batch"
                    );
                    // ECS focus policy is authoritative and must be applied before the matching
                    // pointer press establishes Smithay's implicit grab. Requests made during an
                    // older grab are queued by the host and retried when that grab ends.
                    for action in surface_actions {
                        loop_data.server.apply_surface_action(action);
                    }
                    for effect in input_effects {
                        loop_data.server.apply_input_effect(effect);
                    }
                    for command in host_commands {
                        command_exit_requested |=
                            apply_host_command(&mut children, &loop_data.server, command)?;
                    }
                }
                if let Some(appearance) = cursor_update.appearance {
                    loop_data.server.set_shell_cursor(appearance);
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
                loop_data.server.flush_pending_resizes();
                if work.render_composition {
                    let mut compositions =
                        shell.render_outputs(vec![CompositionOutputRequest {
                            output: OutputId::new(1),
                            destination: CompositionDestination::Owned,
                        }])?;
                    let composition = compositions
                        .pop()
                        .context("nested composition returned no output frame")?;
                    completed_composition = Some(composition.frame);
                    pending_presentation_id = Some(loop_data.server.stage_frame_callbacks());
                    frame_state.composition_rendered(update_now);
                    request_next_composition = bevy_requested_redraw;
                } else {
                    frame_state.application_advanced(update_now);
                }
            }

            if let Some(image) = loop_data.server.take_cursor_image() {
                apply_nested_cursor(&host, image);
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
                let error =
                    "screenshot timed out before a presentable frame was available".to_owned();
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
            if frame_state.presentation_due()
                && let Some(composition) = completed_composition.as_ref()
            {
                let frame = renderer.render(composition.target().view(), capture_path)?;
                if frame.presented {
                    frame_state.presented();
                    if let Some(presentation_id) = pending_presentation_id.take() {
                        loop_data.server.complete_frame_callbacks(presentation_id);
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
            {
                let _flush_span = tracing::trace_span!(
                    target: crate::PROFILE_TARGET,
                    "nested_flush_wayland_clients"
                )
                .entered();
                loop_data.server.flush_clients();
            }
            let mut exit_requested = false;
            while let Some(event) = loop_data.events.pop_front() {
                match event {
                    NestedEvent::Command(command) => {
                        exit_requested |=
                            apply_host_command(&mut children, &loop_data.server, command)?;
                    }
                }
            }
            children.reap();
            if exit_requested {
                break;
            }
        }
        Ok(())
    }))
}

fn apply_host_command(
    children: &mut ChildProcesses,
    server: &ServerState,
    command: HostCommand,
) -> Result<bool> {
    match children.apply(server, command)? {
        HostCommandEffect::Continue => Ok(false),
        HostCommandEffect::Exit => Ok(true),
        HostCommandEffect::AdjustOutputScale(adjustment) => {
            warn!(
                ?adjustment,
                "ignored output-scale shortcut because nested scale is host-owned"
            );
            Ok(false)
        }
        HostCommandEffect::MatchOutputPhysicalScale => {
            warn!("ignored physical-scale match because nested scale is host-owned");
            Ok(false)
        }
    }
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
        frame_state.composition_timeout(now)
    }
}

fn nonzero_size(size: PhysicalSize<u32>) -> PhysicalSize<u32> {
    PhysicalSize::new(size.width.max(1), size.height.max(1))
}

fn apply_nested_cursor(host: &NestedHost, image: CursorImage) {
    let Some(window) = &host.window else {
        return;
    };
    match image {
        CursorImage::Hidden => window.set_cursor_visible(false),
        CursorImage::Named(icon) => {
            window.set_cursor(icon);
            window.set_cursor_visible(true);
        }
        CursorImage::Surface(_) => {
            window.set_cursor(crate::cursor::CursorIcon::Default);
            window.set_cursor_visible(true);
        }
    }
}

struct NestedHost {
    window: Option<Arc<Window>>,
    pending_size: Option<PhysicalSize<u32>>,
    pending_scale_factor: Option<f64>,
    input: NestedAdapter,
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
            input: NestedAdapter::default(),
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
            event => self
                .input
                .handle_window_event(event, self.scale_factor, self.event_time()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BEVY_SETTLE_COMPOSITIONS, FRAME_INTERVAL, FrameState, IterationWork, dispatch_timeout,
        host_work_drained, iteration_work,
    };
    use std::time::Instant;

    fn finish_initial_settle(frame: &mut FrameState, now: Instant) {
        for _ in 0..BEVY_SETTLE_COMPOSITIONS {
            frame.composition_rendered(now);
        }
    }

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
        finish_initial_settle(&mut frame, now);
        frame.presented();
        assert_eq!(
            dispatch_timeout(host_work_drained(false, false, false), &frame, now),
            FRAME_INTERVAL
        );
    }

    #[test]
    fn iteration_work_distinguishes_updates_from_compositions() {
        assert_eq!(
            iteration_work(true, false),
            IterationWork {
                advance_main: true,
                render_composition: false,
            }
        );
        assert_eq!(
            iteration_work(false, true),
            IterationWork {
                advance_main: true,
                render_composition: true,
            }
        );
        assert_eq!(
            iteration_work(false, false),
            IterationWork {
                advance_main: false,
                render_composition: false,
            }
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
        finish_initial_settle(&mut frame, now);
        frame.presented();

        frame.request_present();

        assert!(!frame.composition_dirty());
        assert!(frame.present_needed());
    }

    #[test]
    fn application_update_demand_does_not_dirty_the_cached_composition() {
        let now = Instant::now();
        let mut frame = FrameState::default();
        finish_initial_settle(&mut frame, now);
        frame.presented();

        frame.request_update();

        assert!(frame.update_dirty());
        assert!(!frame.composition_dirty());
        assert!(!frame.update_due(now + FRAME_INTERVAL / 2));
        assert!(frame.update_due(now + FRAME_INTERVAL));

        frame.application_advanced(now + FRAME_INTERVAL);
        assert!(!frame.update_dirty());
        assert!(!frame.composition_dirty());
    }

    #[test]
    fn rendered_composition_remains_pending_until_presented() {
        let now = Instant::now();
        let mut frame = FrameState::default();

        finish_initial_settle(&mut frame, now);

        assert!(!frame.composition_dirty());
        assert!(frame.present_needed());
        assert!(frame.presentation_due());
    }

    #[test]
    fn completed_intermediate_settle_composition_is_presentable() {
        let now = Instant::now();
        let mut frame = FrameState::default();

        assert!(frame.present_needed());
        assert!(!frame.presentation_due());

        frame.composition_rendered(now);

        assert!(frame.presentation_due());
        assert!(frame.settle_compositions_remaining() > 0);
    }

    #[test]
    fn ordinary_demand_does_not_extend_or_cancel_settling() {
        let now = Instant::now();
        let mut frame = FrameState::default();
        frame.composition_rendered(now);
        let remaining = frame.settle_compositions_remaining();

        frame.request_composition();

        assert_eq!(frame.settle_compositions_remaining(), remaining);
        frame.composition_rendered(now);
        assert_eq!(
            frame.settle_compositions_remaining(),
            remaining.saturating_sub(1)
        );
    }

    #[test]
    fn settle_requests_replace_instead_of_accumulating_the_budget() {
        let mut frame = FrameState::default();

        frame.request_settled_composition();
        frame.request_settled_composition();

        assert_eq!(
            frame.settle_compositions_remaining(),
            BEVY_SETTLE_COMPOSITIONS
        );
    }

    #[test]
    fn next_frame_request_survives_presenting_the_current_composition() {
        let now = Instant::now();
        let mut frame = FrameState::default();
        finish_initial_settle(&mut frame, now);

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
        finish_initial_settle(&mut frame, now);

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
        finish_initial_settle(&mut frame, start);
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
