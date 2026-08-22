//! Standalone calloop host around Smithay's DRM output compositor.

use std::{collections::HashMap, time::Instant};

use anyhow::{Context, Result, anyhow, bail};
use calloop::signals::Signals;
use input::Libinput;
use smithay::{
    backend::{
        drm::{DrmEvent, DrmEventMetadata},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        session::{Event as SessionEvent, Session},
    },
    reexports::{calloop::EventLoop, wayland_server::Display},
};
use tracing::{info, warn};

use crate::{
    CompositionDemand, OutputConfiguration, OutputLayout, OutputScale, OutputTopology,
    host::{CompositionDestination, CompositionHost, CompositionOutputRequest, RunOptions},
    input::{RawSeatEvent, RawSeatEventKind, source::libinput::LibinputAdapter},
    runtime::{
        ChildProcesses, FrameState, HostCommandEffect, IterationWork, LoopData, PendingCapture,
        iteration_work, server_mut,
    },
    server::{ServerOptions, ServerState},
};

use super::{
    device::DrmRuntimeBootstrap,
    presentation::{PhysicalDesktop, PhysicalRenderOutcome},
    schedule::PresentationSchedule,
};

enum HostEvent {
    Exit,
    Session(SessionEvent),
    Drm {
        event: DrmEvent,
        metadata: Option<DrmEventMetadata>,
    },
    Input(InputEvent<LibinputInputBackend>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionTarget {
    ActivePhysical,
    InactiveOwned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompositionRoute {
    Physical,
    Owned {
        complete_callbacks: bool,
        capture: bool,
    },
}

#[derive(Default)]
struct FrameCallbackReadiness(bool);

impl FrameCallbackReadiness {
    fn composition_rendered(&mut self, presentation_requested: bool) {
        self.0 |= presentation_requested;
    }

    const fn can_stage(&self) -> bool {
        self.0
    }

    fn reconcile(&mut self, presentation_requested: bool) {
        self.0 &= presentation_requested;
    }
}

const fn composition_route(target: SessionTarget, capture_ready: bool) -> CompositionRoute {
    match (target, capture_ready) {
        (SessionTarget::ActivePhysical, false) => CompositionRoute::Physical,
        (SessionTarget::ActivePhysical, true) => CompositionRoute::Owned {
            complete_callbacks: false,
            capture: true,
        },
        (SessionTarget::InactiveOwned, capture) => CompositionRoute::Owned {
            complete_callbacks: true,
            capture,
        },
    }
}

fn apply_physical_outcome(
    outcome: PhysicalRenderOutcome,
    work: IterationWork,
    now: Instant,
    frame_state: &mut FrameState,
    target: &mut SessionTarget,
) {
    if outcome.inactive {
        *target = SessionTarget::InactiveOwned;
        if work.advance_main {
            frame_state.application_advanced(now);
        }
        frame_state.request_composition();
    } else if !outcome.queued.is_empty() {
        if work.render_composition {
            // This deadline remains a no-vblank fallback. Physical frame
            // retirement replaces it with the DRM vblank clock.
            frame_state.composition_rendered(now);
        }
    } else if !outcome.retry.is_empty() || !outcome.busy.is_empty() {
        if work.advance_main {
            frame_state.application_advanced(now);
        }
    } else {
        if work.render_composition {
            frame_state.composition_rendered(now);
        }
        frame_state.presented();
    }
}

fn primary_output_index(
    outputs: impl IntoIterator<Item = crate::OutputId>,
    primary: crate::OutputId,
) -> Option<usize> {
    outputs.into_iter().position(|output| output == primary)
}

pub(super) fn run(
    bootstrap: DrmRuntimeBootstrap,
    options: RunOptions,
    signals: Signals,
    mut application: Box<dyn CompositionHost>,
) -> Result<()> {
    let DrmRuntimeBootstrap {
        mut session,
        session_notifier,
        drm_notifier,
        output_manager,
        render_state,
        selected_outputs,
        dmabuf_capabilities,
        dmabuf_sources,
        dmabuf_release_source,
    } = bootstrap;
    let started_at = Instant::now();
    let mut calloop: EventLoop<'static, LoopData<HostEvent>> =
        EventLoop::try_new().context("failed to create the DRM calloop event loop")?;
    let display = Display::<ServerState>::new().context("failed to create the Wayland display")?;
    let server = ServerState::new(
        &calloop.handle(),
        display,
        dmabuf_release_source,
        server_mut::<HostEvent>,
        ServerOptions {
            started_at,
            seat_name: "weld-seat0",
            outputs: selected_outputs
                .iter()
                .map(|output| output.definition.clone())
                .collect(),
            dmabuf_capabilities: dmabuf_capabilities.as_ref(),
            dmabuf_sources,
        },
    )?;
    let mut loop_data = LoopData::new(server);
    let mut desktop = PhysicalDesktop::new(
        output_manager,
        render_state,
        &selected_outputs,
        &loop_data.server,
    )?;

    calloop
        .handle()
        .insert_source(signals, |_, _, data| {
            data.events.push_back(HostEvent::Exit);
        })
        .context("failed to register process signals")?;
    calloop
        .handle()
        .insert_source(session_notifier, |event, _, data| {
            data.events.push_back(HostEvent::Session(event));
        })
        .map_err(|_| anyhow!("failed to register libseat notifications"))?;
    calloop
        .handle()
        .insert_source(drm_notifier, |event, metadata, data| {
            data.events.push_back(HostEvent::Drm {
                event,
                metadata: *metadata,
            });
        })
        .map_err(|_| anyhow!("failed to register DRM notifications"))?;

    let seat_name = session.seat();
    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<_>>(session.clone().into());
    libinput_context
        .udev_assign_seat(&seat_name)
        .map_err(|_| anyhow!("failed to assign libinput to seat {seat_name}"))?;
    calloop
        .handle()
        .insert_source(
            LibinputInputBackend::new(libinput_context.clone()),
            |event, _, data| data.events.push_back(HostEvent::Input(event)),
        )
        .map_err(|_| anyhow!("failed to register libinput"))?;

    let mut current_configurations = selected_outputs
        .iter()
        .map(|output| output.configuration)
        .collect::<Vec<_>>();
    let topology = OutputTopology::new(OutputLayout::new(1, current_configurations.clone())?);
    let mut input = LibinputAdapter::new(topology);
    let initial_input = input.initial_event();
    let _ = application.enqueue_input_event(initial_input.clone());
    loop_data.server.forward_raw_input(initial_input);
    desktop.set_cursor_position(input.pointer_position());

    let mut children = ChildProcesses::default();
    let child_requested = children.spawn_requested(&loop_data.server, &options.client)?;
    let mut pending_capture = options
        .screenshot
        .map(|path| PendingCapture::startup(path, child_requested));
    let fastest_interval = selected_outputs
        .iter()
        .map(|output| output.frame_interval)
        .min()
        .context("DRM startup contains no output frame interval")?;
    let mut frame_state = FrameState::with_interval(fastest_interval);
    let mut presentation_schedule = PresentationSchedule::new(
        selected_outputs
            .iter()
            .map(|output| (output.id, output.frame_interval)),
    );
    let mut output_layout_revision = 1_u64;
    let mut target = if session.is_active() {
        SessionTarget::ActivePhysical
    } else {
        SessionTarget::InactiveOwned
    };
    let owned_request = selected_outputs
        .iter()
        .map(|output| CompositionOutputRequest {
            output: output.id,
            destination: CompositionDestination::Owned,
        })
        .collect::<Vec<_>>();
    let mut owned_frames = Vec::with_capacity(selected_outputs.len());
    let mut input_pending = false;
    let mut next_input_update = Instant::now();
    let mut next_remote_service = Instant::now();
    let mut last_vblank_at = None;
    let mut output_vblank_at = HashMap::new();
    let mut output_vblank_sequence = HashMap::new();
    let mut frame_callbacks = FrameCallbackReadiness::default();
    let mut exit_requested = false;

    info!(
        socket = ?loop_data.server.socket_name,
        outputs = selected_outputs.len(),
        primary = %selected_outputs[0].head.name(),
        "Weld DRM compositor is ready"
    );
    while !exit_requested {
        let now = Instant::now();
        if input_pending && now >= next_input_update {
            frame_state.request_update();
        }
        let mut timeout = frame_state.composition_timeout(now);
        if target == SessionTarget::ActivePhysical
            && let Some(presentation_timeout) = presentation_schedule.timeout(now)
        {
            timeout = timeout.min(presentation_timeout);
        }
        calloop
            .dispatch(Some(timeout), &mut loop_data)
            .context("Smithay DRM calloop dispatch failed")?;
        while let Some(event) = loop_data.events.pop_front() {
            match event {
                HostEvent::Exit => exit_requested = true,
                HostEvent::Session(SessionEvent::PauseSession) => {
                    target = SessionTarget::InactiveOwned;
                    for event in input.cancel_active_input().into_iter().flatten() {
                        let _ = application.enqueue_input_event(event.clone());
                        loop_data.server.forward_raw_input(event);
                    }
                    let focus_lost = RawSeatEvent::new(
                        RawSeatEventKind::HostFocusLost,
                        input.last_event_time_msec(),
                    );
                    let _ = application.enqueue_input_event(focus_lost.clone());
                    loop_data.server.forward_raw_input(focus_lost);
                    libinput_context.suspend();
                    desktop.pause();
                }
                HostEvent::Session(SessionEvent::ActivateSession) => {
                    libinput_context
                        .resume()
                        .map_err(|_| anyhow!("failed to resume libinput after VT activation"))?;
                    desktop.activate(&mut loop_data.server)?;
                    presentation_schedule.activate_all();
                    target = SessionTarget::ActivePhysical;
                    frame_state.request_composition();
                }
                HostEvent::Drm {
                    event: DrmEvent::VBlank(crtc),
                    metadata,
                } => {
                    let vblank_at = Instant::now();
                    let sequence = metadata.map(|event| event.sequence);
                    let retired = desktop.retire(crtc, &mut loop_data.server)?;
                    if let Some((output, retired)) = retired {
                        let sequence_delta = sequence
                            .zip(output_vblank_sequence.get(&output).copied())
                            .map(|(current, previous)| current.wrapping_sub(previous));
                        let wall_interval = output_vblank_at
                            .get(&output)
                            .map(|previous| vblank_at.saturating_duration_since(*previous));
                        presentation_schedule.retired(output, retired.deferred_present);
                        match target {
                            SessionTarget::ActivePhysical => {
                                frame_state.physical_frame_retired();
                                if input_pending {
                                    frame_state.request_update();
                                    next_input_update = vblank_at + fastest_interval;
                                }
                            }
                            SessionTarget::InactiveOwned => frame_state.presented(),
                        }
                        if retired.deferred_present {
                            frame_state.request_present();
                        }
                        tracing::trace!(
                            target: "weld_drm_pacing",
                            ?output,
                            ?sequence,
                            ?sequence_delta,
                            wall_interval_micros = ?wall_interval.map(|interval| interval.as_micros()),
                            deferred_present = retired.deferred_present,
                            "retired physical frame"
                        );
                        output_vblank_at.insert(output, vblank_at);
                        if let Some(sequence) = sequence {
                            output_vblank_sequence.insert(output, sequence);
                        }
                    } else {
                        tracing::trace!(
                            target: "weld_drm_pacing",
                            ?crtc,
                            ?sequence,
                            "ignored vblank for an unknown or stale CRTC"
                        );
                    }
                    last_vblank_at = Some(vblank_at);
                }
                HostEvent::Drm {
                    event: DrmEvent::Error(error),
                    ..
                } => return Err(error).context("Smithay DRM notifier failed"),
                HostEvent::Input(event) => {
                    for event in input.convert(event).into_iter().flatten() {
                        if matches!(event.event, RawSeatEventKind::PointerMotion { .. }) {
                            desktop.set_cursor_position(input.pointer_position());
                            presentation_schedule.request_present_all();
                            frame_state.request_present();
                        }
                        if application.enqueue_input_event(event.clone()) {
                            loop_data.server.forward_raw_input(event);
                        }
                        input_pending = true;
                    }
                }
            }
        }

        if loop_data.server.has_surface_events() {
            for event in loop_data.server.take_surface_events() {
                let demand = application.enqueue_surface_event(event);
                match demand {
                    CompositionDemand::Ordinary => {
                        frame_state.request_composition();
                        presentation_schedule.request_composition_all();
                    }
                    CompositionDemand::Settle => {
                        frame_state.request_settled_composition();
                        presentation_schedule.request_composition_all();
                    }
                }
            }
        }
        if loop_data.server.presentation_requested() {
            match target {
                SessionTarget::ActivePhysical => {
                    frame_state.request_present();
                    presentation_schedule.request_present_all();
                }
                SessionTarget::InactiveOwned => frame_state.request_composition(),
            }
        }
        let now = Instant::now();
        if options.remote_debug_enabled && now >= next_remote_service {
            application.service_remote_debug();
            next_remote_service = now + crate::runtime::REMOTE_DEBUG_MAINTENANCE_INTERVAL;
        }
        let work = iteration_work(
            frame_state.update_due(now),
            frame_state.composition_due(now),
        );
        let mut bevy_requested_redraw = false;
        if work.advance_main {
            bevy_requested_redraw =
                application.advance_main(started_at.elapsed().as_millis() as u32);
            input_pending = false;
            next_input_update = match target {
                SessionTarget::ActivePhysical => last_vblank_at
                    .map(|vblank| vblank + fastest_interval)
                    .unwrap_or(now + fastest_interval),
                SessionTarget::InactiveOwned => now + fastest_interval,
            };
            for action in application.take_surface_actions() {
                loop_data.server.apply_surface_action(action);
            }
            for effect in application.take_input_effects() {
                loop_data.server.apply_input_effect(effect);
            }
            for command in application.take_host_commands() {
                match children.apply(&loop_data.server, command)? {
                    HostCommandEffect::Continue => {}
                    HostCommandEffect::Exit => exit_requested = true,
                    HostCommandEffect::AdjustOutputScale(adjustment) => {
                        let output = input.output_at_pointer().or_else(|| {
                            current_configurations
                                .iter()
                                .find(|output| output.is_primary())
                                .map(|output| output.id())
                        });
                        let scale = output.and_then(|output| {
                            current_configurations
                                .iter()
                                .find(|configuration| configuration.id() == output)
                                .and_then(|configuration| configuration.scale().adjust(adjustment))
                                .map(|scale| (output, scale))
                        });
                        if let Some((output, scale)) = scale {
                            output_layout_revision = output_layout_revision.saturating_add(1);
                            OutputScaleUpdate {
                                selected: &selected_outputs,
                                current: &mut current_configurations,
                                layout_revision: output_layout_revision,
                                desktop: &mut desktop,
                                server: &mut loop_data.server,
                                application: application.as_mut(),
                                input: &mut input,
                            }
                            .apply(output, scale)?;
                            frame_state.request_composition();
                            presentation_schedule.request_composition_all();
                        }
                    }
                    HostCommandEffect::MatchOutputPhysicalScale => {
                        let target = input
                            .output_at_pointer()
                            .and_then(|id| {
                                current_configurations
                                    .iter()
                                    .copied()
                                    .find(|output| output.id() == id)
                            })
                            .or_else(|| {
                                current_configurations
                                    .iter()
                                    .copied()
                                    .find(|output| output.is_primary())
                            });
                        let matched = target.and_then(|target| {
                            current_configurations
                                .iter()
                                .copied()
                                .filter(|reference| reference.id() != target.id())
                                .find_map(|reference| {
                                    super::output::scale_matching_physical_density(
                                        target, reference,
                                    )
                                    .map(|scale| (target.id(), scale))
                                })
                        });
                        if let Some((output, scale)) = matched {
                            output_layout_revision = output_layout_revision.saturating_add(1);
                            OutputScaleUpdate {
                                selected: &selected_outputs,
                                current: &mut current_configurations,
                                layout_revision: output_layout_revision,
                                desktop: &mut desktop,
                                server: &mut loop_data.server,
                                application: application.as_mut(),
                                input: &mut input,
                            }
                            .apply(output, scale)?;
                            frame_state.request_composition();
                            presentation_schedule.request_composition_all();
                        } else {
                            warn!("physical scale matching needs two measured outputs");
                        }
                    }
                }
            }
            let cursor_update = application.take_cursor_update();
            if let Some(configuration) = cursor_update.configuration {
                desktop.set_cursor_configuration(configuration)?;
                presentation_schedule.request_present_all();
                frame_state.request_present();
            }
            if let Some(appearance) = cursor_update.appearance {
                loop_data.server.set_shell_cursor(appearance);
            }
            if let Some(image) = loop_data.server.take_cursor_image() {
                desktop.set_cursor_image(image)?;
                presentation_schedule.request_present_all();
                frame_state.request_present();
            }
            if let Some(vt) = application.take_virtual_terminal_switch_request() {
                target = SessionTarget::InactiveOwned;
                session
                    .change_vt(vt)
                    .with_context(|| format!("failed to switch to VT {vt}"))?;
            }
            loop_data.server.flush_pending_resizes();
            if application.should_exit() {
                exit_requested = true;
            }
        }

        let capture_ready = pending_capture
            .as_ref()
            .is_some_and(|capture| !capture.wait_for_client || application.has_surface_frame());
        let mut capture_forced_owned = false;
        if work.render_composition {
            frame_callbacks.composition_rendered(loop_data.server.presentation_requested());
            match composition_route(target, capture_ready) {
                CompositionRoute::Owned {
                    complete_callbacks,
                    capture,
                } => {
                    application.render_outputs(&owned_request, &mut owned_frames)?;
                    let primary = selected_outputs[0].id;
                    let frame_index = primary_output_index(
                        owned_frames.iter().map(|frame| frame.output),
                        primary,
                    )
                    .context("owned DRM composition returned no primary frame")?;
                    let frame = &owned_frames[frame_index];
                    if complete_callbacks && loop_data.server.presentation_requested() {
                        let presentation_id = loop_data.server.stage_frame_callbacks();
                        loop_data.server.complete_frame_callbacks(presentation_id);
                    }
                    if capture {
                        let capture = pending_capture
                            .take()
                            .context("ready capture request disappeared")?;
                        let result = desktop
                            .capture_owned(&frame.frame, &capture.path)
                            .map_err(|error| error.to_string());
                        match capture.remote_request_id {
                            Some(request_id) => application.complete_capture(request_id, result),
                            None => {
                                result.map_err(anyhow::Error::msg)?;
                                desktop.pause();
                                return Ok(());
                            }
                        }
                        capture_forced_owned = target == SessionTarget::ActivePhysical;
                    }
                }
                CompositionRoute::Physical => {
                    desktop.request_all_compositions();
                    presentation_schedule.request_composition_all();
                }
            }
        }
        let due_outputs = presentation_schedule.due_outputs(now);
        let physical_due = target == SessionTarget::ActivePhysical
            && !capture_forced_owned
            && !due_outputs.is_empty();
        if capture_forced_owned {
            frame_state.composition_rendered(now);
            frame_state.presented();
            frame_state.request_composition();
        } else if physical_due {
            let stage_callbacks = frame_callbacks.can_stage();
            let vblank_phases = due_outputs
                .iter()
                .filter_map(|output| {
                    output_vblank_at
                        .get(output)
                        .map(|vblank| (*output, now.saturating_duration_since(*vblank)))
                })
                .collect::<HashMap<_, _>>();
            let outcome = desktop.render(
                &due_outputs,
                application.as_mut(),
                &mut loop_data.server,
                &vblank_phases,
                stage_callbacks,
            )?;
            let unavailable_outputs = desktop.unavailable_output_ids().collect::<Vec<_>>();
            presentation_schedule.unavailable(&unavailable_outputs);
            presentation_schedule.queued(&outcome.queued);
            presentation_schedule.completed_without_queue(&outcome.empty, now);
            presentation_schedule.retry_after_interval(&outcome.retry, now);
            apply_physical_outcome(outcome, work, now, &mut frame_state, &mut target);
        } else if work.render_composition {
            frame_state.composition_rendered(now);
            frame_state.presented();
        } else if work.advance_main {
            frame_state.application_advanced(now);
        }
        frame_callbacks.reconcile(loop_data.server.presentation_requested());

        if bevy_requested_redraw {
            frame_state.request_composition();
            presentation_schedule.request_composition_all();
        }
        if pending_capture.is_none()
            && let Some(request) = application.take_capture_request()
        {
            pending_capture = Some(PendingCapture::remote(request.request_id, request.path));
            frame_state.request_composition();
        }
        if pending_capture
            .as_ref()
            .is_some_and(|capture| capture.deadline <= Instant::now())
        {
            let capture = pending_capture
                .take()
                .context("pending capture disappeared")?;
            let error = "screenshot timed out before an owned frame was available".to_owned();
            if let Some(request_id) = capture.remote_request_id {
                application.complete_capture(request_id, Err(error));
            } else {
                bail!("startup {error}");
            }
        }
        children.reap();
        loop_data.server.flush_clients();
    }
    desktop.pause();
    Ok(())
}

struct OutputScaleUpdate<'a> {
    selected: &'a [super::output::SelectedOutput],
    current: &'a mut Vec<OutputConfiguration>,
    layout_revision: u64,
    desktop: &'a mut PhysicalDesktop,
    server: &'a mut ServerState,
    application: &'a mut dyn CompositionHost,
    input: &'a mut LibinputAdapter,
}

impl OutputScaleUpdate<'_> {
    fn apply(self, output_id: crate::OutputId, scale: OutputScale) -> Result<()> {
        let output = self
            .current
            .iter_mut()
            .find(|output| output.id() == output_id)
            .context("scaled output is not configured")?;
        *output = output.with_scale(scale)?;
        super::output::center_primary_below_others(self.current)?;
        for configuration in self.current.iter().copied() {
            let selected = self
                .selected
                .iter()
                .find(|selected| selected.id == configuration.id())
                .context("configured output is not backed by DRM")?;
            let metrics = super::output::metrics_for_configuration(configuration, selected.mode)?;
            self.server.update_output_metrics(
                configuration.id(),
                metrics,
                (
                    super::output::logical_coordinate(configuration.position().x)?,
                    super::output::logical_coordinate(configuration.position().y)?,
                ),
            );
            self.desktop
                .update_configuration(configuration.id(), configuration)?;
        }
        self.application.update_output_topology(self.current);
        let topology = OutputTopology::new(OutputLayout::new(
            self.layout_revision,
            self.current.clone(),
        )?);
        if let Some(event) = self.input.update_output_topology(topology) {
            let _ = self.application.enqueue_input_event(event.clone());
            self.server.forward_raw_input(event);
        }
        self.desktop
            .set_cursor_position(self.input.pointer_position());
        self.desktop.request_all_compositions();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::runtime::{FrameState, IterationWork};

    use super::{
        CompositionRoute, FrameCallbackReadiness, PhysicalRenderOutcome, SessionTarget,
        apply_physical_outcome, composition_route, primary_output_index,
    };

    #[test]
    fn composition_route_keeps_physical_presentation_except_for_owned_work() {
        assert_eq!(
            composition_route(SessionTarget::ActivePhysical, false),
            CompositionRoute::Physical
        );
        assert_eq!(
            composition_route(SessionTarget::ActivePhysical, true),
            CompositionRoute::Owned {
                complete_callbacks: false,
                capture: true,
            }
        );
        assert_eq!(
            composition_route(SessionTarget::InactiveOwned, false),
            CompositionRoute::Owned {
                complete_callbacks: true,
                capture: false,
            }
        );
    }

    #[test]
    fn primary_capture_selection_does_not_depend_on_frame_order() {
        let primary = crate::OutputId::new(1);
        let outputs = [crate::OutputId::new(2), primary];
        assert_eq!(primary_output_index(outputs, primary), Some(1));
    }

    #[test]
    fn frame_callbacks_wait_for_composition_and_remain_ready_until_staged() {
        let mut readiness = FrameCallbackReadiness::default();
        readiness.reconcile(true);
        assert!(!readiness.can_stage());

        readiness.composition_rendered(true);
        assert!(readiness.can_stage());
        readiness.reconcile(true);
        assert!(readiness.can_stage());

        readiness.reconcile(false);
        assert!(!readiness.can_stage());
    }

    #[test]
    fn busy_physical_output_paces_main_without_discarding_composition() {
        let mut frame_state = FrameState::with_interval(Duration::from_millis(16));
        let mut target = SessionTarget::ActivePhysical;
        apply_physical_outcome(
            PhysicalRenderOutcome {
                busy: vec![crate::OutputId::new(1)],
                ..Default::default()
            },
            IterationWork {
                advance_main: true,
                render_composition: true,
            },
            Instant::now(),
            &mut frame_state,
            &mut target,
        );

        assert_eq!(target, SessionTarget::ActivePhysical);
        assert!(!frame_state.update_dirty());
        assert!(frame_state.composition_dirty());
        assert!(frame_state.present_needed());
    }

    #[test]
    fn inactive_physical_output_routes_pending_work_to_owned_composition() {
        let mut frame_state = FrameState::with_interval(Duration::from_millis(16));
        let mut target = SessionTarget::ActivePhysical;
        apply_physical_outcome(
            PhysicalRenderOutcome {
                inactive: true,
                ..Default::default()
            },
            IterationWork {
                advance_main: true,
                render_composition: true,
            },
            Instant::now(),
            &mut frame_state,
            &mut target,
        );

        assert_eq!(target, SessionTarget::InactiveOwned);
        assert!(frame_state.update_dirty());
        assert!(frame_state.composition_dirty());
        assert!(frame_state.present_needed());
    }
}
