//! Standalone libseat, udev, libinput, and Smithay GBM/KMS backend.

use std::{
    ops::{Deref, DerefMut},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use calloop::{
    channel::{self, Event as ChannelEvent},
    signals::Signals,
};
use smithay::{
    backend::{
        drm::{DrmDevice, DrmEvent},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent},
    },
    reexports::{
        calloop::{
            EventLoop as CalloopEventLoop, LoopHandle as CalloopLoopHandle, RegistrationToken,
        },
        drm::control::connector,
        input::Libinput,
        rustix::fs::Dev,
        wayland_server::Display,
    },
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{debug, error, info, trace, warn};

use crate::{
    OutputScale,
    cursor::CursorHostUpdate,
    dmabuf::DmabufSourceCache,
    host::{
        CompositionDemand, CompositionHost, CompositionTargetId, CompositionTargets, PreparedHost,
        RenderContext, RunOptions,
    },
    input::{
        InputPosition, RawPointerUpdate, RawSeatEvent, RawSeatEventKind,
        source::libinput::LibinputAdapter,
    },
    renderer::{CursorOverlay, GpuCursor, read_composition_rgba, write_png},
    runtime::{
        ChildProcesses, FrameState, HostCommand, HostCommandEffect, LoopData, PendingCapture,
        REMOTE_DEBUG_MAINTENANCE_INTERVAL, iteration_work, server_mut,
    },
    server::{OutputMetrics, ServerOptions, ServerState},
};

mod discovery;
mod gpu;
mod presenter;

use discovery::{DrmDeviceDiscovery, connector_name, discover_output, output_description};
use gpu::DrmGpu;
use presenter::{PresenterEvent, PresenterHandle};

const PRESENTER_SHUTDOWN_DEADLINE: Duration = Duration::from_millis(250);

enum DrmRuntimeEvent {
    Input(RawSeatEvent),
    Session(SessionEvent),
    Udev(UdevEvent),
    Drm(DrmEvent),
    Presenter(PresenterEvent),
    Command(HostCommand),
}

/// Owns the registered DRM device through libseat-managed shutdown.
///
/// This guard must remain a host-loop local and must never be dropped from a
/// calloop callback, where [`CalloopLoopHandle::remove`] would borrow the source
/// registry recursively.
struct RegisteredDrmDevice<'event_loop> {
    handle: CalloopLoopHandle<'event_loop, LoopData<DrmRuntimeEvent, LibinputAdapter>>,
    token: RegistrationToken,
    device: DrmDevice,
}

impl Deref for RegisteredDrmDevice<'_> {
    type Target = DrmDevice;

    fn deref(&self) -> &Self::Target {
        &self.device
    }
}

impl DerefMut for RegisteredDrmDevice<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.device
    }
}

impl Drop for RegisteredDrmDevice<'_> {
    fn drop(&mut self) {
        self.handle.remove(self.token);
        self.device.pause();
    }
}

struct OutputMonitor {
    scanner: DrmScanner,
    device_id: Dev,
    device_path: PathBuf,
    connector: connector::Handle,
    connected: bool,
    mode_compatible: bool,
    event_source_healthy: bool,
}

struct DrmHostCommandContext<'a> {
    children: &'a mut ChildProcesses,
    server: &'a mut ServerState,
    input_adapter: &'a mut LibinputAdapter,
    events: &'a mut std::collections::VecDeque<DrmRuntimeEvent>,
    shell: &'a mut dyn CompositionHost,
    target: wgpu::TextureView,
    extent: crate::surface::Extent,
    output_metrics: &'a mut OutputMetrics,
    frame_state: &'a mut FrameState,
}

struct DrmCursor {
    gpu: GpuCursor,
    overlay: CursorOverlay,
    animation_deadline: Option<Instant>,
    position: Option<InputPosition>,
}

impl DrmCursor {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, output_scale: f64, now: Instant) -> Self {
        Self {
            gpu: GpuCursor::new(
                device,
                queue,
                crate::cursor::CursorConfiguration::default(),
                output_scale,
                now,
            ),
            overlay: CursorOverlay::hidden(),
            animation_deadline: None,
            position: None,
        }
    }

    fn observe_input(&mut self, event: &RawSeatEvent) -> bool {
        let Some(update) = event.pointer_update() else {
            return false;
        };
        let position = match update {
            RawPointerUpdate::Position(position) => Some(position),
            RawPointerUpdate::Clear => None,
        };
        if self.position == position {
            return false;
        }
        self.position = position;
        true
    }

    fn apply_host_update(
        &mut self,
        server: &mut ServerState,
        update: CursorHostUpdate,
        now: Instant,
    ) {
        if let Some(configuration) = update.configuration {
            self.gpu.set_configuration(configuration, now);
        }
        if let Some(appearance) = update.appearance {
            server.set_shell_cursor(appearance);
        }
    }

    fn refresh(
        &mut self,
        server: &mut ServerState,
        output_scale: f64,
        frame_state: &mut FrameState,
        now: Instant,
    ) {
        if let Some(image) = server.take_cursor_image() {
            self.gpu.set_image(image, now);
        }
        self.gpu.set_position(self.position);
        self.gpu.set_output_scale(output_scale);
        let evaluated = self.gpu.evaluate(now);
        self.animation_deadline = evaluated.next_animation;
        if evaluated.overlay != self.overlay {
            self.overlay = evaluated.overlay;
            frame_state.request_present();
        }
    }
}

fn apply_host_command(context: DrmHostCommandContext<'_>, command: HostCommand) -> Result<bool> {
    let DrmHostCommandContext {
        children,
        server,
        input_adapter,
        events,
        shell,
        target,
        extent,
        output_metrics,
        frame_state,
    } = context;
    match children.apply(server, command)? {
        HostCommandEffect::Continue => Ok(false),
        HostCommandEffect::Exit => Ok(true),
        HostCommandEffect::AdjustOutputScale(adjustment) => {
            let current = match OutputScale::new(output_metrics.scale_factor()) {
                Ok(current) => current,
                Err(error) => {
                    warn!(%error, "ignored output-scale adjustment from invalid current state");
                    return Ok(false);
                }
            };
            let Some(next_scale) = current.adjust(adjustment) else {
                return Ok(false);
            };
            let candidate = match output_metrics.with_scale(next_scale) {
                Ok(candidate) => candidate,
                Err(error) => {
                    warn!(
                        scale_factor = next_scale.value(),
                        %error,
                        "ignored invalid runtime output scale"
                    );
                    return Ok(false);
                }
            };

            let logical_width = f64::from(candidate.physical_width()) / candidate.scale_factor();
            let logical_height = f64::from(candidate.physical_height()) / candidate.scale_factor();
            server.update_output_metrics(candidate);
            if let Some(event) = input_adapter.update_output_bounds(logical_width, logical_height) {
                events.push_back(DrmRuntimeEvent::Input(event));
            }
            shell.set_output_geometry(target, extent, candidate.scale_factor());
            *output_metrics = candidate;
            frame_state.request_settled_composition();
            info!(
                scale_factor = candidate.scale_factor(),
                "updated standalone output scale"
            );
            Ok(false)
        }
    }
}

impl OutputMonitor {
    const fn physical_available(&self, session_active: bool) -> bool {
        session_active && self.connected && self.mode_compatible && self.event_source_healthy
    }

    fn handle(
        &mut self,
        event: UdevEvent,
        drm: &mut DrmDevice,
        presenter: &mut PresenterHandle,
        session_active: bool,
    ) {
        match event {
            UdevEvent::Changed { device_id } if device_id == self.device_id => {
                let scan = match self.scanner.scan_connectors(drm) {
                    Ok(scan) => scan,
                    Err(error) => {
                        warn!(%error, "failed to rescan the selected DRM device");
                        return;
                    }
                };
                for event in scan {
                    match event {
                        DrmScanEvent::Disconnected { connector, .. }
                            if connector.handle() == self.connector =>
                        {
                            self.connected = false;
                            presenter.suspend();
                            warn!("active DRM connector disconnected; composition remains live");
                        }
                        DrmScanEvent::Connected { connector, .. }
                            if connector.handle() == self.connector =>
                        {
                            self.connected = true;
                            if self.physical_available(session_active) {
                                if let Err(error) = drm.reset_state() {
                                    error!(%error, "failed to reset DRM state after connector recovery");
                                } else if let Err(error) = presenter.activate_after_session() {
                                    error!(%error, "failed to reactivate GBM/KMS presentation");
                                } else {
                                    info!(
                                        "active DRM connector reconnected; GBM/KMS presentation restored"
                                    );
                                }
                            } else if !self.mode_compatible {
                                warn!(
                                    "active DRM connector reconnected with changed modes; restart is still required"
                                );
                            }
                        }
                        DrmScanEvent::Changed { connector, .. }
                            if connector.handle() == self.connector =>
                        {
                            self.connected = false;
                            self.mode_compatible = false;
                            presenter.suspend();
                            warn!(
                                "active connector modes changed; physical presentation is unavailable until restart"
                            );
                        }
                        _ => {}
                    }
                }
            }
            UdevEvent::Removed { device_id } if device_id == self.device_id => {
                self.connected = false;
                presenter.suspend();
                drm.pause();
                error!(
                    path = %self.device_path.display(),
                    "selected DRM device was removed; composition remains live"
                );
            }
            UdevEvent::Added { device_id, path } => {
                warn!(?device_id, path = %path.display(), "additional DRM GPUs are not supported yet");
            }
            UdevEvent::Changed { .. } | UdevEvent::Removed { .. } => {}
        }
    }
}

pub(crate) fn prepare(options: RunOptions, signals: Signals) -> Result<PreparedHost> {
    let _prepare_span =
        tracing::trace_span!(target: crate::PROFILE_TARGET, "drm_backend_prepare").entered();

    let started_at = Instant::now();
    let mut calloop: CalloopEventLoop<'static, LoopData<DrmRuntimeEvent, LibinputAdapter>> =
        CalloopEventLoop::try_new().context("failed to create the DRM calloop event loop")?;

    let (session, session_notifier) =
        LibSeatSession::new().context("failed to acquire a libseat session")?;
    let seat_name = session.seat();
    let udev = UdevBackend::new(&seat_name).context("failed to initialize DRM udev discovery")?;
    let device = DrmDeviceDiscovery::new(session, udev.device_list())?;
    let output = discover_output(&device.drm)?;
    let selected_connector_name = connector_name(&output.connector);
    let (output_descriptor, mut output_metrics) =
        output_description(&output.connector, output.mode, options.output_scale)?;
    let logical_width = f64::from(output_metrics.physical_width()) / output_metrics.scale_factor();
    let logical_height =
        f64::from(output_metrics.physical_height()) / output_metrics.scale_factor();
    let input_adapter = LibinputAdapter::new(logical_width, logical_height);
    let (mut drm_device, drm_notifier) = DrmDevice::new(device.drm.clone(), true)
        .context("failed to initialize Smithay DRM device")?;
    let drm_surface = drm_device
        .create_surface(output.crtc, output.mode, &[output.connector.handle()])
        .context("failed to create Smithay DRM output surface")?;
    let gpu = DrmGpu::new(device.device_id, &device.device_path)?;
    let refresh_millihertz = u32::try_from(smithay::output::Mode::from(output.mode).refresh)
        .context("DRM mode has a negative refresh rate")?;
    let dmabuf_sources = DmabufSourceCache::new(&gpu.device);
    let capture_device = gpu.device.clone();
    let capture_queue = gpu.queue.clone();
    let (dmabuf_release_sender, dmabuf_release_source) = channel::channel();

    let display = Display::<ServerState>::new().context("failed to create the Wayland display")?;
    let server = ServerState::new(
        &calloop.handle(),
        display,
        dmabuf_release_source,
        server_mut::<DrmRuntimeEvent, LibinputAdapter>,
        ServerOptions {
            started_at,
            seat_name: &seat_name,
            output_descriptor,
            output_metrics,
            dmabuf_capabilities: gpu.dmabuf_capabilities.as_ref(),
            dmabuf_sources: dmabuf_sources.clone(),
        },
    )?;
    let mut loop_data = LoopData::with_state(server, input_adapter);

    calloop
        .handle()
        .insert_source(signals, |event, _, data| {
            debug!(signal = ?event.signal(), "received shutdown signal");
            data.events
                .push_back(DrmRuntimeEvent::Command(HostCommand::Exit));
        })
        .context("failed to register process signals")?;

    let mut libinput_context = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(
        device.session.clone().into(),
    );
    libinput_context
        .udev_assign_seat(&seat_name)
        .map_err(|error| anyhow!("failed to assign libinput to {seat_name}: {error:?}"))?;
    let input_backend = LibinputInputBackend::new(libinput_context.clone());
    loop_data.events.push_back(DrmRuntimeEvent::Input(
        loop_data.backend_state.initial_event(),
    ));
    calloop
        .handle()
        .insert_source(input_backend, |event, _, data| {
            for event in data.backend_state.convert(event).into_iter().flatten() {
                data.events.push_back(DrmRuntimeEvent::Input(event));
            }
        })
        .map_err(|_| anyhow!("failed to register libinput"))?;

    let mut session_input = libinput_context.clone();
    calloop
        .handle()
        .insert_source(session_notifier, move |event, _, data| {
            match event {
                SessionEvent::PauseSession => session_input.suspend(),
                SessionEvent::ActivateSession => {
                    if let Err(error) = session_input.resume() {
                        error!(?error, "failed to resume libinput");
                    }
                }
            }
            data.events.push_back(DrmRuntimeEvent::Session(event));
        })
        .map_err(|_| anyhow!("failed to register libseat notifications"))?;

    calloop
        .handle()
        .insert_source(udev, |event, _, data| {
            data.events.push_back(DrmRuntimeEvent::Udev(event));
        })
        .map_err(|_| anyhow!("failed to register udev notifications"))?;

    let drm_notifier_registration = calloop
        .handle()
        .insert_source(drm_notifier, |event, _, data| {
            data.events.push_back(DrmRuntimeEvent::Drm(event));
        })
        .map_err(|_| anyhow!("failed to register DRM page-flip notifications"))?;

    let mut targets = CompositionTargets::new(
        &gpu.device,
        crate::surface::Extent::new(
            output_metrics.physical_width(),
            output_metrics.physical_height(),
        ),
    );
    let context = RenderContext {
        instance: gpu.instance.clone(),
        adapter: gpu.adapter.clone(),
        device: gpu.device.clone(),
        queue: gpu.queue.clone(),
        dmabuf: crate::dmabuf::DmabufContext::new(dmabuf_release_sender, dmabuf_sources),
        extent: targets.extent(),
        scale_factor: output_metrics.scale_factor(),
        initial_target: targets.view(CompositionTargetId::FIRST).clone(),
    };
    let DrmDeviceDiscovery {
        session,
        drm,
        device_id,
        device_path,
    } = device;
    let crtc = output.crtc;
    let scanner = output.scanner;
    let connector = output.connector.handle();

    Ok(PreparedHost::new(context, move |host| {
        let mut shell = host;
        // These host-loop locals deliberately drop in reverse declaration order:
        // presenter (and DrmSurface), registered DrmDevice, then the remaining
        // weak session handle and fd clone. RegisteredDrmDevice removes its
        // notifier and pauses before releasing the device, preventing Smithay
        // from replaying file-scoped KMS object IDs captured from the previous
        // session. The captured calloop and its LibSeatSessionNotifier drop last.
        // This ordering remains important for notifier, fd, and seat teardown
        // even though device restoration is deliberately suppressed.
        let mut session_owner = session;
        let drm = drm;
        let mut drm_device = RegisteredDrmDevice {
            handle: calloop.handle(),
            token: drm_notifier_registration,
            device: drm_device,
        };
        let (presenter_events, presenter_source) = channel::channel();
        calloop
            .handle()
            .insert_source(presenter_source, |event, _, data| match event {
                ChannelEvent::Msg(event) => {
                    data.events.push_back(DrmRuntimeEvent::Presenter(event))
                }
                ChannelEvent::Closed => data
                    .events
                    .push_back(DrmRuntimeEvent::Presenter(PresenterEvent::Stopped)),
            })
            .map_err(|_| anyhow!("failed to register GBM/KMS presenter results"))?;
        let mut presenter =
            PresenterHandle::spawn(gpu, drm_surface, drm.clone(), crtc, presenter_events)?;
        let mut output_monitor = OutputMonitor {
            scanner,
            device_id,
            device_path,
            connector,
            connected: true,
            mode_compatible: true,
            event_source_healthy: true,
        };
        let mut children = ChildProcesses::default();
        let child_requested = children.spawn_requested(&loop_data.server, &options.client)?;
        let mut pending_capture = options
            .screenshot
            .map(|path| PendingCapture::startup(path, child_requested));
        let remote_debug_enabled = options.remote_debug_enabled;
        let mut frame_state = FrameState::default().with_refresh_millihertz(refresh_millihertz);
        let mut next_remote_service = Instant::now();
        let mut cursor = DrmCursor::new(
            &capture_device,
            &capture_queue,
            output_metrics.scale_factor(),
            Instant::now(),
        );
        let mut session_active = true;
        info!(
            socket = ?loop_data.server.socket_name,
            connector = %selected_connector_name,
            "Weld GBM/KMS compositor is ready"
        );
        let mut exit_requested = false;
        while !exit_requested {
            let now = Instant::now();
            let timeout = dispatch_timeout(DispatchTimeoutContext {
                frame_state: &frame_state,
                capture: pending_capture.as_ref(),
                remote_debug_enabled,
                physical_output_available: output_monitor.physical_available(session_active),
                cursor_animation: cursor.animation_deadline,
                now,
            });
            {
                let _dispatch_span = tracing::trace_span!(
                    target: crate::PROFILE_TARGET,
                    "drm_calloop_wait_and_dispatch"
                )
                .entered();
                calloop
                    .dispatch(timeout, &mut loop_data)
                    .context("DRM calloop dispatch failed")?;
            }

            let mut input_pending = false;
            let mut cursor_position_changed = false;
            if !loop_data.events.is_empty() {
                let _events_span = tracing::trace_span!(
                    target: crate::PROFILE_TARGET,
                    "drm_runtime_event_drain"
                )
                .entered();
                let mut event_counts = [0_usize; 6];
                while let Some(event) = loop_data.events.pop_front() {
                    match event {
                        DrmRuntimeEvent::Input(event) => {
                            event_counts[0] += 1;
                            input_pending = true;
                            cursor_position_changed |= cursor.observe_input(&event);
                            if shell.enqueue_input_event(event.clone()) {
                                loop_data.server.forward_raw_input(event);
                            }
                        }
                        DrmRuntimeEvent::Session(SessionEvent::PauseSession) => {
                            event_counts[1] += 1;
                            session_active = false;
                            presenter.suspend();
                            drm_device.pause();
                            input_pending = true;
                            for event in loop_data
                                .backend_state
                                .cancel_active_input()
                                .into_iter()
                                .flatten()
                            {
                                if shell.enqueue_input_event(event.clone()) {
                                    loop_data.server.forward_raw_input(event);
                                }
                            }
                            let focus_lost = RawSeatEvent::new(
                                RawSeatEventKind::HostFocusLost,
                                loop_data.backend_state.last_event_time_msec(),
                            );
                            cursor_position_changed |= cursor.observe_input(&focus_lost);
                            if shell.enqueue_input_event(focus_lost.clone()) {
                                loop_data.server.forward_raw_input(focus_lost);
                            }
                            info!("libseat session paused; physical presentation suspended");
                        }
                        DrmRuntimeEvent::Session(SessionEvent::ActivateSession) => {
                            event_counts[1] += 1;
                            let recovering = !session_active;
                            session_active = true;
                            if recovering {
                                match drm_device.activate(true) {
                                    Ok(()) if output_monitor.physical_available(session_active) => {
                                        match presenter.activate_after_session() {
                                            Ok(()) => frame_state.request_present(),
                                            Err(error) => error!(
                                                %error,
                                                "failed to restore GBM/KMS presentation"
                                            ),
                                        }
                                    }
                                    Ok(()) => {}
                                    Err(error) => error!(
                                        %error,
                                        "failed to reactivate Smithay DRM device"
                                    ),
                                }
                            }
                            info!("libseat session activated; GBM/KMS recovery processed");
                        }
                        DrmRuntimeEvent::Udev(event) => {
                            event_counts[2] += 1;
                            output_monitor.handle(
                                event,
                                &mut drm_device,
                                &mut presenter,
                                session_active,
                            );
                            if output_monitor.physical_available(session_active) {
                                frame_state.request_present();
                            }
                        }
                        DrmRuntimeEvent::Drm(DrmEvent::VBlank(crtc)) => {
                            event_counts[3] += 1;
                            presenter.frame_submitted(crtc);
                        }
                        DrmRuntimeEvent::Drm(DrmEvent::Error(error)) => {
                            event_counts[3] += 1;
                            output_monitor.event_source_healthy = false;
                            presenter.suspend();
                            drm_device.pause();
                            error!(%error, "DRM page-flip event source failed; physical output requires restart");
                        }
                        DrmRuntimeEvent::Presenter(event) => {
                            event_counts[4] += 1;
                            log_presenter_event(&event);
                            presenter.handle_event(&event);
                        }
                        DrmRuntimeEvent::Command(command) => {
                            event_counts[5] += 1;
                            // Signal commands currently only request exit. Keep them on the
                            // shared path so future host commands cannot bypass backend policy.
                            let target =
                                host_composition_target(&targets, presenter.in_flight_target());
                            exit_requested |= apply_host_command(
                                DrmHostCommandContext {
                                    children: &mut children,
                                    server: &mut loop_data.server,
                                    input_adapter: &mut loop_data.backend_state,
                                    events: &mut loop_data.events,
                                    shell: shell.as_mut(),
                                    target: targets.view(target).clone(),
                                    extent: targets.extent(),
                                    output_metrics: &mut output_metrics,
                                    frame_state: &mut frame_state,
                                },
                                command,
                            )?;
                        }
                    }
                }
                tracing::trace!(
                    target: crate::PROFILE_TARGET,
                    input = event_counts[0],
                    session = event_counts[1],
                    udev = event_counts[2],
                    drm = event_counts[3],
                    presenter = event_counts[4],
                    command = event_counts[5],
                    "DRM runtime event batch"
                );
            }
            if exit_requested {
                break;
            }

            if cursor_position_changed {
                cursor.refresh(
                    &mut loop_data.server,
                    output_metrics.scale_factor(),
                    &mut frame_state,
                    Instant::now(),
                );
                if frame_state.presentation_due()
                    && output_monitor.physical_available(session_active)
                {
                    let target = targets.completed();
                    presenter.offer(target, targets.view(target).clone(), cursor.overlay.clone());
                    frame_state.presented();
                }
            }

            if input_pending {
                frame_state.request_composition();
            }

            if loop_data.server.has_surface_events() {
                let _surface_span = tracing::trace_span!(
                    target: crate::PROFILE_TARGET,
                    "drm_host_surface_ingress"
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
                frame_state.request_composition();
            }

            let now = Instant::now();
            if remote_debug_enabled && now >= next_remote_service {
                shell.service_remote_debug();
                next_remote_service = now + REMOTE_DEBUG_MAINTENANCE_INTERVAL;
            }

            let work = iteration_work(frame_state.composition_due(now));
            let mut bevy_requested_redraw = false;
            if work.advance_main {
                bevy_requested_redraw = shell.advance_main(started_at.elapsed().as_millis() as u32);
                let surface_actions = shell.take_surface_actions();
                let input_effects = shell.take_input_effects();
                let host_commands = shell.take_host_commands();
                let cursor_update = shell.take_cursor_update();
                let virtual_terminal = shell.take_virtual_terminal_switch_request();
                if !surface_actions.is_empty()
                    || !input_effects.is_empty()
                    || !host_commands.is_empty()
                {
                    let _results_span = tracing::trace_span!(
                        target: crate::PROFILE_TARGET,
                        "drm_apply_ecs_results"
                    )
                    .entered();
                    tracing::trace!(
                        target: crate::PROFILE_TARGET,
                        surface_actions = surface_actions.len(),
                        input_effects = input_effects.len(),
                        host_commands = host_commands.len(),
                        "ECS result batch"
                    );
                    for action in surface_actions {
                        loop_data.server.apply_surface_action(action);
                    }
                    for effect in input_effects {
                        loop_data.server.apply_input_effect(effect);
                    }
                    for command in host_commands {
                        let target =
                            host_composition_target(&targets, presenter.in_flight_target());
                        exit_requested |= apply_host_command(
                            DrmHostCommandContext {
                                children: &mut children,
                                server: &mut loop_data.server,
                                input_adapter: &mut loop_data.backend_state,
                                events: &mut loop_data.events,
                                shell: shell.as_mut(),
                                target: targets.view(target).clone(),
                                extent: targets.extent(),
                                output_metrics: &mut output_metrics,
                                frame_state: &mut frame_state,
                            },
                            command,
                        )?;
                    }
                }
                if let Some(virtual_terminal) = virtual_terminal {
                    let _virtual_terminal_span = tracing::trace_span!(
                        target: crate::PROFILE_TARGET,
                        "apply_virtual_terminal_request"
                    )
                    .entered();
                    presenter.suspend();
                    drm_device.pause();
                    match session_owner.change_vt(virtual_terminal) {
                        Ok(()) => {
                            session_active = false;
                            info!(virtual_terminal, "requested virtual-terminal switch");
                        }
                        Err(error) => {
                            warn!(
                                virtual_terminal,
                                %error,
                                "failed to switch virtual terminal"
                            );
                            if output_monitor.physical_available(session_active) {
                                match drm_device.activate(true) {
                                    Ok(()) => {
                                        if let Err(error) = presenter.activate_after_session() {
                                            error!(
                                                %error,
                                                "failed to restore GBM/KMS after rejected VT switch"
                                            );
                                        }
                                    }
                                    Err(activate_error) => error!(
                                        %activate_error,
                                        "failed to reactivate DRM after rejected VT switch"
                                    ),
                                }
                            }
                        }
                    }
                }
                cursor.apply_host_update(&mut loop_data.server, cursor_update, now);
                if shell.should_exit() || exit_requested {
                    break;
                }
            }

            cursor.refresh(
                &mut loop_data.server,
                output_metrics.scale_factor(),
                &mut frame_state,
                now,
            );

            if work.advance_main {
                loop_data.server.flush_pending_resizes();
                let target = host_composition_target(&targets, presenter.in_flight_target());
                shell.render_composition(targets.view(target).clone(), targets.extent())?;
                targets.mark_completed(target);
                let callback_batch = loop_data.server.stage_frame_callbacks();
                loop_data.server.complete_frame_callbacks(callback_batch);
                frame_state.composition_rendered(now);

                let capture_ready = pending_capture
                    .as_ref()
                    .is_some_and(|capture| !capture.wait_for_client || shell.has_surface_frame());
                if capture_ready && let Some(capture) = pending_capture.take() {
                    let _capture_span = tracing::trace_span!(
                        target: crate::PROFILE_TARGET,
                        "capture_readback_encode"
                    )
                    .entered();
                    let result = read_composition_rgba(
                        &capture_device,
                        &capture_queue,
                        targets.texture(target),
                        output_metrics.physical_width(),
                        output_metrics.physical_height(),
                    )
                    .and_then(|pixels| {
                        write_png(
                            &capture.path,
                            output_metrics.physical_width(),
                            output_metrics.physical_height(),
                            &pixels,
                        )
                    })
                    .map_err(|error| error.to_string());
                    exit_requested |= complete_capture(shell.as_mut(), capture, result)?;
                }
                if output_monitor.physical_available(session_active) {
                    presenter.offer(target, targets.view(target).clone(), cursor.overlay.clone());
                    frame_state.presented();
                }
            }

            if pending_capture.is_none()
                && let Some(request) = shell.take_capture_request()
            {
                pending_capture = Some(PendingCapture::remote(request.request_id, request.path));
                frame_state.request_composition();
            }
            if pending_capture
                .as_ref()
                .is_some_and(|capture| capture.deadline <= Instant::now())
                && let Some(capture) = pending_capture.take()
            {
                exit_requested |= complete_capture(
                    shell.as_mut(),
                    capture,
                    Err("screenshot timed out before a composition was available".to_owned()),
                )?;
            }
            if work.advance_main && bevy_requested_redraw {
                frame_state.request_composition();
            }
            if frame_state.presentation_due() && output_monitor.physical_available(session_active) {
                let target = targets.completed();
                presenter.offer(target, targets.view(target).clone(), cursor.overlay.clone());
                frame_state.presented();
            }
            {
                let _flush_span = tracing::trace_span!(
                    target: crate::PROFILE_TARGET,
                    "drm_flush_wayland_clients"
                )
                .entered();
                loop_data.server.flush_clients();
            }
            children.reap();
        }

        shutdown_presenter(&mut calloop, &mut loop_data, &mut presenter)?;
        Ok(())
    }))
}

fn shutdown_presenter(
    calloop: &mut CalloopEventLoop<'static, LoopData<DrmRuntimeEvent, LibinputAdapter>>,
    loop_data: &mut LoopData<DrmRuntimeEvent, LibinputAdapter>,
    presenter: &mut PresenterHandle,
) -> Result<()> {
    presenter.begin_shutdown();
    let deadline = Instant::now() + PRESENTER_SHUTDOWN_DEADLINE;
    while !presenter.stopped() && Instant::now() < deadline {
        calloop
            .dispatch(
                Some(deadline.saturating_duration_since(Instant::now())),
                loop_data,
            )
            .context("DRM calloop dispatch failed during presenter shutdown")?;
        while let Some(event) = loop_data.events.pop_front() {
            if let DrmRuntimeEvent::Presenter(event) = event {
                log_presenter_event(&event);
                presenter.handle_event(&event);
            }
        }
    }
    presenter.join_if_finished();
    if !presenter.stopped() {
        warn!(
            timeout_milliseconds = PRESENTER_SHUTDOWN_DEADLINE.as_millis(),
            "GBM/KMS presenter did not stop promptly; detaching until process teardown"
        );
    }
    Ok(())
}

fn host_composition_target(
    targets: &CompositionTargets,
    worker_target: Option<CompositionTargetId>,
) -> CompositionTargetId {
    let [first, second] = targets.ids();
    if worker_target == Some(first) {
        second
    } else {
        first
    }
}

struct DispatchTimeoutContext<'a> {
    frame_state: &'a FrameState,
    capture: Option<&'a PendingCapture>,
    remote_debug_enabled: bool,
    physical_output_available: bool,
    cursor_animation: Option<Instant>,
    now: Instant,
}

fn dispatch_timeout(context: DispatchTimeoutContext<'_>) -> Option<std::time::Duration> {
    let DispatchTimeoutContext {
        frame_state,
        capture,
        remote_debug_enabled,
        physical_output_available,
        cursor_animation,
        now,
    } = context;
    let composition = frame_state.composition_demand_timeout(now);
    let remote_debug = remote_debug_enabled.then_some(REMOTE_DEBUG_MAINTENANCE_INTERVAL);
    let capture = capture.map(|capture| capture.deadline.saturating_duration_since(now));
    let cursor = physical_output_available
        .then(|| cursor_animation.map(|deadline| deadline.saturating_duration_since(now)))
        .flatten();
    [composition, remote_debug, capture, cursor]
        .into_iter()
        .flatten()
        .min()
}

fn log_presenter_event(event: &PresenterEvent) {
    match event {
        PresenterEvent::Ready { epoch } => info!(epoch, "GBM/KMS presenter is ready"),
        PresenterEvent::Frame(frame) => {
            trace!(?frame, "GBM/KMS presenter frame event")
        }
        PresenterEvent::OutputUnavailable(message) => {
            error!(%message, "physical DRM presentation is unavailable")
        }
        PresenterEvent::DeviceLost(message) => {
            error!(%message, "GBM/KMS wgpu device was lost")
        }
        PresenterEvent::UncapturedError(message) => {
            error!(%message, "uncaptured error on the shared compositor wgpu device")
        }
        PresenterEvent::Stopped => warn!("GBM/KMS presenter worker stopped"),
    }
}

fn complete_capture(
    shell: &mut dyn CompositionHost,
    capture: PendingCapture,
    result: std::result::Result<(), String>,
) -> Result<bool> {
    match capture.remote_request_id {
        Some(request_id) => {
            shell.complete_capture(request_id, result);
            Ok(false)
        }
        None => {
            result.map_err(anyhow::Error::msg)?;
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DispatchTimeoutContext, FrameState, REMOTE_DEBUG_MAINTENANCE_INTERVAL, dispatch_timeout,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn inactive_remote_debug_uses_its_maintenance_interval() {
        let now = Instant::now();
        let mut frame_state = FrameState::default();
        for _ in 0..crate::runtime::BEVY_SETTLE_COMPOSITIONS {
            frame_state.composition_rendered(now);
        }
        frame_state.presented();
        let timeout = dispatch_timeout(DispatchTimeoutContext {
            frame_state: &frame_state,
            capture: None,
            remote_debug_enabled: true,
            physical_output_available: false,
            cursor_animation: None,
            now,
        });

        assert_eq!(timeout, Some(REMOTE_DEBUG_MAINTENANCE_INTERVAL));
    }

    #[test]
    fn inactive_output_keeps_demand_driven_composition_live() {
        let frame_state = FrameState::default();
        let timeout = dispatch_timeout(DispatchTimeoutContext {
            frame_state: &frame_state,
            capture: None,
            remote_debug_enabled: false,
            physical_output_available: false,
            cursor_animation: Some(Instant::now()),
            now: Instant::now(),
        });

        assert_eq!(timeout, Some(Duration::ZERO));
    }

    #[test]
    fn active_overdue_composition_dispatches_immediately() {
        let frame_state = FrameState::default();
        let timeout = dispatch_timeout(DispatchTimeoutContext {
            frame_state: &frame_state,
            capture: None,
            remote_debug_enabled: false,
            physical_output_available: true,
            cursor_animation: None,
            now: Instant::now(),
        });

        assert_eq!(timeout, Some(Duration::ZERO));
    }

    #[test]
    fn cursor_animation_wakes_only_an_available_output() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(12);
        let mut frame = FrameState::default();
        for _ in 0..crate::runtime::BEVY_SETTLE_COMPOSITIONS {
            frame.composition_rendered(now);
        }
        frame.presented();

        assert_eq!(
            dispatch_timeout(DispatchTimeoutContext {
                frame_state: &frame,
                capture: None,
                remote_debug_enabled: false,
                physical_output_available: true,
                cursor_animation: Some(deadline),
                now,
            }),
            Some(Duration::from_millis(12))
        );
        assert_eq!(
            dispatch_timeout(DispatchTimeoutContext {
                frame_state: &frame,
                capture: None,
                remote_debug_enabled: false,
                physical_output_available: false,
                cursor_animation: Some(deadline),
                now,
            }),
            None
        );
    }
}
