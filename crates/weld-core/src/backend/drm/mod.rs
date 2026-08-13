//! Standalone libseat, udev, libinput, and direct-wgpu DRM backend.

use std::{
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
        drm::DrmDeviceFd,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent},
    },
    reexports::{
        calloop::EventLoop as CalloopEventLoop, drm::control::connector, input::Libinput,
        rustix::fs::Dev, wayland_server::Display,
    },
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{debug, error, info, warn};

use crate::{
    dmabuf::DmabufSourceCache,
    host::{
        CompositionDemand, CompositionHost, CompositionTargetId, CompositionTargets, PreparedHost,
        RenderContext, RunOptions,
    },
    input::{RawSeatEvent, RawSeatEventKind, source::libinput::LibinputAdapter},
    renderer::{CursorOverlay, read_composition_rgba, write_png},
    runtime::{
        ChildProcesses, FrameState, HostCommand, LoopData, PendingCapture, iteration_work,
        server_mut,
    },
    server::{ServerOptions, ServerState},
};

mod direct;
mod discovery;
mod presenter;

use direct::DirectDrmGpu;
use discovery::{DrmDeviceDiscovery, connector_name, discover_output, output_description};
use presenter::{FrameOutcome, PresenterEvent, PresenterHandle};

const REMOTE_DEBUG_MAINTENANCE_INTERVAL: Duration = Duration::from_millis(100);

const PRESENTER_SHUTDOWN_DEADLINE: Duration = Duration::from_millis(250);

enum DrmRuntimeEvent {
    Input(RawSeatEvent),
    Session(SessionEvent),
    Udev(UdevEvent),
    Presenter(PresenterEvent),
    Command(HostCommand),
}

struct OutputMonitor {
    drm: DrmDeviceFd,
    scanner: DrmScanner,
    device_id: Dev,
    device_path: PathBuf,
    connector: connector::Handle,
    connected: bool,
    mode_compatible: bool,
}

impl OutputMonitor {
    fn handle(&mut self, event: UdevEvent, presenter: &mut PresenterHandle, session_active: bool) {
        match event {
            UdevEvent::Changed { device_id } if device_id == self.device_id => {
                let scan = match self.scanner.scan_connectors(&self.drm) {
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
                            if self.mode_compatible && session_active {
                                presenter.activate();
                                info!("active DRM connector reconnected; reconfiguring presenter");
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
    let mut calloop: CalloopEventLoop<'static, LoopData<DrmRuntimeEvent>> =
        CalloopEventLoop::try_new().context("failed to create the DRM calloop event loop")?;

    let (session, session_notifier) =
        LibSeatSession::new().context("failed to acquire a libseat session")?;
    let seat_name = session.seat();
    let udev = UdevBackend::new(&seat_name).context("failed to initialize DRM udev discovery")?;
    let device = DrmDeviceDiscovery::new(session, udev.device_list())?;
    let output = discover_output(&device.drm)?;
    let selected_connector_name = connector_name(&output.connector);
    let (output_descriptor, output_metrics) = output_description(&output.connector, output.mode)?;
    let direct_gpu = DirectDrmGpu::new(
        &device.drm,
        device.device_id,
        &device.device_path,
        &output.connector,
        output.mode,
    )?;
    let refresh_millihertz = direct_gpu.mode.refresh_millihertz;
    let dmabuf_sources = DmabufSourceCache::new(&direct_gpu.device);
    let capture_device = direct_gpu.device.clone();
    let capture_queue = direct_gpu.queue.clone();
    let (dmabuf_release_sender, dmabuf_release_source) = channel::channel();

    let display = Display::<ServerState>::new().context("failed to create the Wayland display")?;
    let server = ServerState::new(
        &calloop.handle(),
        display,
        dmabuf_release_source,
        server_mut::<DrmRuntimeEvent>,
        ServerOptions {
            started_at,
            seat_name: &seat_name,
            output_descriptor,
            output_metrics,
            dmabuf_capabilities: direct_gpu.dmabuf_capabilities.as_ref(),
            dmabuf_sources: dmabuf_sources.clone(),
        },
    )?;
    let mut loop_data = LoopData::new(server);

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
    let logical_width = f64::from(output_metrics.physical_width()) / output_metrics.scale_factor();
    let logical_height =
        f64::from(output_metrics.physical_height()) / output_metrics.scale_factor();
    let mut input_adapter = LibinputAdapter::new(logical_width, logical_height);
    loop_data
        .events
        .push_back(DrmRuntimeEvent::Input(input_adapter.initial_event()));
    calloop
        .handle()
        .insert_source(input_backend, move |event, _, data| {
            if let Some(event) = input_adapter.convert(event) {
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

    let mut targets = CompositionTargets::new(
        &direct_gpu.device,
        crate::surface::Extent::new(
            output_metrics.physical_width(),
            output_metrics.physical_height(),
        ),
    );
    let context = RenderContext {
        instance: direct_gpu.instance.clone(),
        adapter: direct_gpu.adapter.clone(),
        device: direct_gpu.device.clone(),
        queue: direct_gpu.queue.clone(),
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
    let scanner = output.scanner;
    let connector = output.connector.handle();

    Ok(PreparedHost::new(context, move |host| {
        let mut shell = host;
        // Declared before the presenter so Rust drops the worker first. A
        // worker stuck in native acquisition still retains its own fd clone
        // until process teardown; the host never closes the libseat session
        // underneath it. These explicit locals avoid relying on closure
        // capture-field drop order.
        let mut session_owner = session;
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
            .map_err(|_| anyhow!("failed to register direct DRM presenter results"))?;
        let mut presenter = PresenterHandle::spawn(direct_gpu, presenter_events)?;
        let mut output_monitor = OutputMonitor {
            drm,
            scanner,
            device_id,
            device_path,
            connector,
            connected: true,
            mode_compatible: true,
        };
        let mut children = ChildProcesses::default();
        let child_requested = children.spawn_requested(&loop_data.server, &options.client)?;
        let mut pending_capture = options
            .screenshot
            .map(|path| PendingCapture::startup(path, child_requested));
        let remote_debug_enabled = options.remote_debug_enabled;
        let mut frame_state = FrameState::default().with_refresh_millihertz(refresh_millihertz);
        let mut cursor = CursorOverlay::default();
        let mut session_active = true;
        info!(
            socket = ?loop_data.server.socket_name,
            connector = %selected_connector_name,
            "Weld direct DRM compositor is ready"
        );
        let mut exit_requested = false;
        while !exit_requested {
            let now = Instant::now();
            let timeout = dispatch_timeout(
                &frame_state,
                pending_capture.as_ref(),
                remote_debug_enabled,
                session_active,
                now,
            );
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
            if !loop_data.events.is_empty() {
                let _events_span = tracing::trace_span!(
                    target: crate::PROFILE_TARGET,
                    "drm_runtime_event_drain"
                )
                .entered();
                let mut event_counts = [0_usize; 5];
                while let Some(event) = loop_data.events.pop_front() {
                    match event {
                        DrmRuntimeEvent::Input(event) => {
                            event_counts[0] += 1;
                            input_pending = true;
                            shell.enqueue_input_event(event);
                        }
                        DrmRuntimeEvent::Session(SessionEvent::PauseSession) => {
                            event_counts[1] += 1;
                            session_active = false;
                            presenter.suspend();
                            input_pending = true;
                            shell.enqueue_input_event(RawSeatEvent::new(
                                RawSeatEventKind::HostFocusLost,
                                started_at.elapsed().as_millis() as u32,
                            ));
                            info!("libseat session paused; physical presentation suspended");
                        }
                        DrmRuntimeEvent::Session(SessionEvent::ActivateSession) => {
                            event_counts[1] += 1;
                            session_active = true;
                            if output_monitor.connected && output_monitor.mode_compatible {
                                presenter.activate();
                            }
                            info!("libseat session activated; presenter reconfiguration requested");
                        }
                        DrmRuntimeEvent::Udev(event) => {
                            event_counts[2] += 1;
                            output_monitor.handle(event, &mut presenter, session_active);
                        }
                        DrmRuntimeEvent::Presenter(event) => {
                            event_counts[3] += 1;
                            log_presenter_event(&event);
                            presenter.handle_event(&event);
                        }
                        DrmRuntimeEvent::Command(command) => {
                            event_counts[4] += 1;
                            exit_requested |= children.apply(&loop_data.server, command)?;
                        }
                    }
                }
                tracing::trace!(
                    target: crate::PROFILE_TARGET,
                    input = event_counts[0],
                    session = event_counts[1],
                    udev = event_counts[2],
                    presenter = event_counts[3],
                    command = event_counts[4],
                    "DRM runtime event batch"
                );
            }
            if exit_requested {
                break;
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
            let work = iteration_work(
                input_pending,
                frame_state.composition_due(now),
                remote_debug_enabled,
                session_active,
            );
            let mut request_next_composition = false;
            if work.advance_main {
                let bevy_requested_redraw = shell.advance_main(
                    started_at.elapsed().as_millis() as u32,
                    work.composition_advance,
                );
                let next_cursor = CursorOverlay::from_logical(
                    shell
                        .pointer_position()
                        .map(|position| (position.x, position.y)),
                    output_metrics.scale_factor(),
                );
                if next_cursor != cursor {
                    cursor = next_cursor;
                    frame_state.request_present();
                }
                let surface_actions = shell.take_surface_actions();
                let input_effects = shell.take_input_effects();
                let host_commands = shell.take_host_commands();
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
                        exit_requested |= children.apply(&loop_data.server, command)?;
                    }
                }
                if let Some(virtual_terminal) = virtual_terminal {
                    let _virtual_terminal_span = tracing::trace_span!(
                        target: crate::PROFILE_TARGET,
                        "apply_virtual_terminal_request"
                    )
                    .entered();
                    presenter.suspend();
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
                            if session_active
                                && output_monitor.connected
                                && output_monitor.mode_compatible
                            {
                                presenter.activate();
                            }
                        }
                    }
                }
                if bevy_requested_redraw && !work.composition_advance {
                    frame_state.request_composition();
                }
                if shell.should_exit() || exit_requested {
                    break;
                }
                if work.composition_advance {
                    let target = host_composition_target(&targets, presenter.in_flight_target());
                    shell.render_composition(targets.view(target).clone(), targets.extent())?;
                    targets.mark_completed(target);
                    let callback_batch = loop_data.server.stage_frame_callbacks();
                    loop_data.server.complete_frame_callbacks(callback_batch);
                    frame_state.composition_rendered(now);
                    request_next_composition = bevy_requested_redraw;

                    let capture_ready = pending_capture.as_ref().is_some_and(|capture| {
                        !capture.wait_for_client || shell.has_surface_frame()
                    });
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
                    presenter.offer(target, targets.view(target).clone(), cursor);
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
            if request_next_composition {
                frame_state.request_composition();
            }
            if frame_state.presentation_due()
                && session_active
                && output_monitor.connected
                && output_monitor.mode_compatible
            {
                let target = targets.completed();
                presenter.offer(target, targets.view(target).clone(), cursor);
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
    calloop: &mut CalloopEventLoop<'static, LoopData<DrmRuntimeEvent>>,
    loop_data: &mut LoopData<DrmRuntimeEvent>,
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
            "direct DRM presenter did not stop promptly; detaching until process teardown"
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

fn dispatch_timeout(
    frame_state: &FrameState,
    capture: Option<&PendingCapture>,
    remote_debug_enabled: bool,
    session_active: bool,
    now: Instant,
) -> Option<std::time::Duration> {
    let composition = frame_state.composition_demand_timeout(now, session_active);
    let remote_debug = remote_debug_enabled.then_some(REMOTE_DEBUG_MAINTENANCE_INTERVAL);
    let capture = capture.map(|capture| capture.deadline.saturating_duration_since(now));
    [composition, remote_debug, capture]
        .into_iter()
        .flatten()
        .min()
}

fn log_presenter_event(event: &PresenterEvent) {
    match event {
        PresenterEvent::Ready { epoch } => info!(epoch, "direct DRM presenter is ready"),
        PresenterEvent::FrameReleased {
            outcome: FrameOutcome::Presented,
            ..
        } => {}
        PresenterEvent::FrameReleased {
            outcome: FrameOutcome::Unavailable,
            ..
        } => error!("direct DRM frame made physical presentation unavailable"),
        PresenterEvent::FrameReleased { outcome, .. } => {
            debug!(?outcome, "direct DRM frame was not presented")
        }
        PresenterEvent::OutputUnavailable(message) => {
            error!(%message, "physical DRM presentation is unavailable")
        }
        PresenterEvent::DeviceLost(message) => {
            error!(%message, "direct DRM wgpu device was lost")
        }
        PresenterEvent::UncapturedError(message) => {
            error!(%message, "uncaptured error on the shared compositor wgpu device")
        }
        PresenterEvent::Stopped => warn!("direct DRM presenter worker stopped"),
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
    use super::{FrameState, REMOTE_DEBUG_MAINTENANCE_INTERVAL, dispatch_timeout};
    use std::time::{Duration, Instant};

    #[test]
    fn inactive_remote_debug_uses_its_maintenance_interval() {
        let timeout = dispatch_timeout(&FrameState::default(), None, true, false, Instant::now());

        assert_eq!(timeout, Some(REMOTE_DEBUG_MAINTENANCE_INTERVAL));
    }

    #[test]
    fn inactive_composition_demand_does_not_poll_without_maintenance_work() {
        let timeout = dispatch_timeout(&FrameState::default(), None, false, false, Instant::now());

        assert_eq!(timeout, None);
    }

    #[test]
    fn active_overdue_composition_dispatches_immediately() {
        let timeout = dispatch_timeout(&FrameState::default(), None, false, true, Instant::now());

        assert_eq!(timeout, Some(Duration::ZERO));
    }
}
