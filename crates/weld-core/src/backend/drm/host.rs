//! Standalone calloop host around Smithay's DRM output compositor.

use crate::host::CompositionOutputFrame;
use crate::runtime::native::{
    HostState, NativeDriver, NativeRuntime, PolicyFrame, RuntimeIntegration, RuntimeSetup,
};

use std::{collections::HashMap, time::Instant};

use anyhow::{Context, Result, anyhow, bail};
use calloop::signals::Signals;
use input::Libinput;
use smithay::backend::{
    drm::{DrmEvent, DrmEventMetadata},
    input::InputEvent,
    libinput::{LibinputInputBackend, LibinputSessionInterface},
    session::{Event as SessionEvent, Session},
};
use tracing::{info, warn};
use weld_client::{ClientRuntime, ClientRuntimeAdapter};

use crate::{
    CompositionDemand, OutputConfiguration, OutputLayout, OutputScale, OutputTopology,
    host::{
        ApplicationHost, CompositionDestination, CompositionOutputRequest, HostPolicy, RunOptions,
    },
    input::{RawSeatEvent, RawSeatEventKind, source::libinput::LibinputAdapter},
    runtime::{FrameState, HostCommandEffect, IterationWork, PendingCapture, iteration_work},
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
    mut application: Box<dyn ApplicationHost>,
    client_bridge: crate::server::WaylandClientBridge,
    adapters: Vec<ClientRuntimeAdapter>,
    wake_sources: Vec<crate::host::ClientRuntimeWakeSource>,
) -> Result<()> {
    let DrmRuntimeBootstrap {
        session,
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
    let mut runtime = NativeRuntime::prepare(RuntimeSetup {
        server: ServerOptions {
            initial_toplevel_size: None,
            started_at,
            seat_name: "weld-seat0",
            outputs: selected_outputs
                .iter()
                .map(|output| output.definition.clone())
                .collect(),
            dmabuf_capabilities: dmabuf_capabilities.as_ref(),
            dmabuf_sources,
            socket_name: options.socket_name.as_deref(),
            keyboard_repeat_mode: options
                .keyboard_repeat_mode
                .unwrap_or(crate::input::KeyboardRepeatMode::Client),
        },
        releases: dmabuf_release_source,
        bridge: client_bridge,
        adapters,
        wakes: wake_sources,
        signals,
        shutdown_event: || HostEvent::Exit,
    })?;
    let mut desktop = PhysicalDesktop::new(
        output_manager,
        render_state,
        &selected_outputs,
        &runtime.state.data.server,
    )?;

    runtime
        .event_loop
        .handle()
        .insert_source(session_notifier, |event, _, data| {
            data.events.push_back(HostEvent::Session(event));
        })
        .map_err(|_| anyhow!("failed to register libseat notifications"))?;
    runtime
        .event_loop
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
    runtime
        .event_loop
        .handle()
        .insert_source(
            LibinputInputBackend::new(libinput_context.clone()),
            |event, _, data| data.events.push_back(HostEvent::Input(event)),
        )
        .map_err(|_| anyhow!("failed to register libinput"))?;

    let current_configurations = selected_outputs
        .iter()
        .map(|output| output.configuration)
        .collect::<Vec<_>>();
    let topology = OutputTopology::new(OutputLayout::new(1, current_configurations.clone())?);
    let input = LibinputAdapter::new(topology);
    let initial_input = input.initial_event();
    if application.enqueue_input_event(initial_input.clone()) {
        runtime
            .state
            .clients
            .dispatch_unconsumed_input(initial_input.into_runtime());
        runtime.state.data.server.apply_pending_client_work();
    }
    desktop.set_cursor_position(input.pointer_position());

    let child_requested = runtime
        .state
        .children
        .spawn_requested(&runtime.state.data.server, &options.client)?;
    let pending_capture = options
        .screenshot
        .map(|path| PendingCapture::startup(path, child_requested));
    let fastest_interval = selected_outputs
        .iter()
        .map(|output| output.frame_interval)
        .min()
        .context("DRM startup contains no output frame interval")?;
    let frame_state = FrameState::with_interval(fastest_interval);
    let presentation_schedule = PresentationSchedule::new(
        selected_outputs
            .iter()
            .map(|output| (output.id, output.frame_interval)),
    );
    let output_layout_revision = 1_u64;
    let target = if session.is_active() {
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
    let owned_frames = Vec::with_capacity(selected_outputs.len());
    let input_pending = false;
    let next_input_update = Instant::now();
    let next_remote_service = Instant::now();
    let last_vblank_at = None;
    let output_vblank_at = HashMap::new();
    let output_vblank_sequence = HashMap::new();
    let frame_callbacks = FrameCallbackReadiness::default();
    let exit_requested = false;

    info!(
        socket = ?runtime.state.data.server.socket_name,
        outputs = selected_outputs.len(),
        primary = %selected_outputs[0].head.name(),
        "Weld DRM compositor is ready"
    );
    let driver = DrmDriver {
        session,
        desktop,
        selected_outputs,
        libinput_context,
        current_configurations,
        input,
        pending_capture,
        fastest_interval,
        frame_state,
        presentation_schedule,
        output_layout_revision,
        target,
        owned_request,
        owned_frames,
        input_pending,
        next_input_update,
        next_remote_service,
        last_vblank_at,
        output_vblank_at,
        output_vblank_sequence,
        frame_callbacks,
        exit_requested,
        started_at,
        remote_debug_enabled: options.remote_debug_enabled,
    };
    runtime.run(
        RuntimeIntegration::Native {
            application,
            driver: Box::new(driver),
        },
        fastest_interval,
    )
}

struct DrmDriver {
    session: smithay::backend::session::libseat::LibSeatSession,
    desktop: PhysicalDesktop,
    selected_outputs: Vec<super::output::SelectedOutput>,
    libinput_context: Libinput,
    current_configurations: Vec<OutputConfiguration>,
    input: LibinputAdapter,
    pending_capture: Option<PendingCapture>,
    fastest_interval: std::time::Duration,
    frame_state: FrameState,
    presentation_schedule: PresentationSchedule,
    output_layout_revision: u64,
    target: SessionTarget,
    owned_request: Vec<CompositionOutputRequest>,
    owned_frames: Vec<CompositionOutputFrame>,
    input_pending: bool,
    next_input_update: Instant,
    next_remote_service: Instant,
    last_vblank_at: Option<Instant>,
    output_vblank_at: HashMap<crate::OutputId, Instant>,
    output_vblank_sequence: HashMap<crate::OutputId, u32>,
    frame_callbacks: FrameCallbackReadiness,
    exit_requested: bool,
    started_at: Instant,
    remote_debug_enabled: bool,
}

impl NativeDriver<HostEvent> for DrmDriver {
    fn prepare_dispatch(
        &mut self,
        _: &mut HostState<HostEvent>,
        _: &mut dyn ApplicationHost,
    ) -> Result<bool> {
        let now = Instant::now();
        if self.input_pending && now >= self.next_input_update {
            self.frame_state.request_update();
        }
        Ok(!self.exit_requested)
    }
    fn timeout(&self, now: Instant) -> std::time::Duration {
        let mut timeout = self.frame_state.composition_timeout(now);
        if self.target == SessionTarget::ActivePhysical
            && let Some(presentation_timeout) = self.presentation_schedule.timeout(now)
        {
            timeout = timeout.min(presentation_timeout);
        }
        timeout
    }
    fn dispatched(
        &mut self,
        state: &mut HostState<HostEvent>,
        application: &mut dyn ApplicationHost,
    ) -> Result<()> {
        while let Some(event) = state.data.events.pop_front() {
            match event {
                HostEvent::Exit => self.exit_requested = true,
                HostEvent::Session(SessionEvent::PauseSession) => {
                    self.target = SessionTarget::InactiveOwned;
                    for event in self.input.cancel_active_input().into_iter().flatten() {
                        if application.enqueue_input_event(event.clone()) {
                            state
                                .clients
                                .dispatch_unconsumed_input(event.into_runtime());
                            state.data.server.apply_pending_client_work();
                        }
                    }
                    let focus_lost = RawSeatEvent::new(
                        RawSeatEventKind::HostFocusLost,
                        self.input.last_event_time_msec(),
                    );
                    let _ = application.enqueue_input_event(focus_lost.clone());
                    state
                        .clients
                        .dispatch_unconsumed_input(focus_lost.into_runtime());
                    state.data.server.apply_pending_client_work();
                    self.libinput_context.suspend();
                    self.desktop.pause();
                }
                HostEvent::Session(SessionEvent::ActivateSession) => {
                    self.libinput_context
                        .resume()
                        .map_err(|_| anyhow!("failed to resume libinput after VT activation"))?;
                    self.desktop
                        .activate(&mut state.data.server, &mut state.callbacks)?;
                    self.presentation_schedule.activate_all();
                    self.target = SessionTarget::ActivePhysical;
                    self.frame_state.request_composition();
                }
                HostEvent::Drm {
                    event: DrmEvent::VBlank(crtc),
                    metadata,
                } => {
                    let vblank_at = Instant::now();
                    let sequence = metadata.map(|event| event.sequence);
                    let retired =
                        self.desktop
                            .retire(crtc, &mut state.data.server, &mut state.callbacks)?;
                    if let Some((output, retired)) = retired {
                        let sequence_delta = sequence
                            .zip(self.output_vblank_sequence.get(&output).copied())
                            .map(|(current, previous)| current.wrapping_sub(previous));
                        let wall_interval = self
                            .output_vblank_at
                            .get(&output)
                            .map(|previous| vblank_at.saturating_duration_since(*previous));
                        self.presentation_schedule
                            .retired(output, retired.deferred_present);
                        match self.target {
                            SessionTarget::ActivePhysical => {
                                self.frame_state.physical_frame_retired();
                                if self.input_pending {
                                    self.frame_state.request_update();
                                    self.next_input_update = vblank_at + self.fastest_interval;
                                }
                            }
                            SessionTarget::InactiveOwned => self.frame_state.presented(),
                        }
                        if retired.deferred_present {
                            self.frame_state.request_present();
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
                        self.output_vblank_at.insert(output, vblank_at);
                        if let Some(sequence) = sequence {
                            self.output_vblank_sequence.insert(output, sequence);
                        }
                    } else {
                        tracing::trace!(
                            target: "weld_drm_pacing",
                            ?crtc,
                            ?sequence,
                            "ignored vblank for an unknown or stale CRTC"
                        );
                    }
                    self.last_vblank_at = Some(vblank_at);
                }
                HostEvent::Drm {
                    event: DrmEvent::Error(error),
                    ..
                } => return Err(error).context("Smithay DRM notifier failed"),
                HostEvent::Input(event) => {
                    for event in self.input.convert(event).into_iter().flatten() {
                        if matches!(event.event, RawSeatEventKind::PointerMotion { .. }) {
                            self.desktop
                                .set_cursor_position(self.input.pointer_position());
                            self.presentation_schedule.request_present_all();
                            self.frame_state.request_present();
                        }
                        if application.enqueue_input_event(event.clone()) {
                            state
                                .clients
                                .dispatch_unconsumed_input(event.into_runtime());
                            state.data.server.apply_pending_client_work();
                        }
                        self.input_pending = true;
                    }
                }
            }
        }

        Ok(())
    }
    fn client_demand(&mut self, demand: CompositionDemand) {
        match demand {
            CompositionDemand::Ordinary => self.frame_state.request_composition(),
            CompositionDemand::Settle => self.frame_state.request_settled_composition(),
        }
        self.presentation_schedule.request_composition_all();
    }
    fn policy_frame(
        &mut self,
        state: &mut HostState<HostEvent>,
        application: &mut dyn ApplicationHost,
    ) -> PolicyFrame {
        if state.data.server.presentation_requested() {
            match self.target {
                SessionTarget::ActivePhysical => {
                    self.frame_state.request_present();
                    self.presentation_schedule.request_present_all();
                }
                SessionTarget::InactiveOwned => self.frame_state.request_composition(),
            }
        }
        let now = Instant::now();
        if self.remote_debug_enabled && now >= self.next_remote_service {
            application.service_remote_debug();
            self.next_remote_service = now + crate::runtime::REMOTE_DEBUG_MAINTENANCE_INTERVAL;
        }
        PolicyFrame {
            now,
            work: iteration_work(
                self.frame_state.update_due(now),
                self.frame_state.composition_due(now),
            ),
            redraw: false,
            input_time: self.started_at.elapsed().as_millis() as u32,
        }
    }
    fn apply_native_effects(
        &mut self,
        state: &mut HostState<HostEvent>,
        application: &mut dyn ApplicationHost,
        frame: &mut PolicyFrame,
    ) -> Result<bool> {
        let now = frame.now;
        self.input_pending = false;
        self.next_input_update = match self.target {
            SessionTarget::ActivePhysical => self
                .last_vblank_at
                .map(|vblank| vblank + self.fastest_interval)
                .unwrap_or(now + self.fastest_interval),
            SessionTarget::InactiveOwned => now + self.fastest_interval,
        };
        for command in application.take_host_commands() {
            match state.children.apply(&state.data.server, command)? {
                HostCommandEffect::Continue => {}
                HostCommandEffect::SetLegacyKeyRepeat(legacy) => {
                    state.data.server.set_legacy_key_repeat(legacy)
                }
                HostCommandEffect::Exit => self.exit_requested = true,
                HostCommandEffect::AdjustOutputScale(adjustment) => {
                    let output = self.input.output_at_pointer().or_else(|| {
                        self.current_configurations
                            .iter()
                            .find(|output| output.is_primary())
                            .map(|output| output.id())
                    });
                    let scale = output.and_then(|output| {
                        self.current_configurations
                            .iter()
                            .find(|configuration| configuration.id() == output)
                            .and_then(|configuration| configuration.scale().adjust(adjustment))
                            .map(|scale| (output, scale))
                    });
                    if let Some((output, scale)) = scale {
                        self.output_layout_revision = self.output_layout_revision.saturating_add(1);
                        OutputScaleUpdate {
                            selected: &self.selected_outputs,
                            current: &mut self.current_configurations,
                            layout_revision: self.output_layout_revision,
                            desktop: &mut self.desktop,
                            server: &mut state.data.server,
                            application,
                            input: &mut self.input,
                            clients: &mut state.clients,
                        }
                        .apply(output, scale)?;
                        self.frame_state.request_composition();
                        self.presentation_schedule.request_composition_all();
                    }
                }
                HostCommandEffect::MatchOutputPhysicalScale => {
                    let target_configuration = self
                        .input
                        .output_at_pointer()
                        .and_then(|id| {
                            self.current_configurations
                                .iter()
                                .copied()
                                .find(|output| output.id() == id)
                        })
                        .or_else(|| {
                            self.current_configurations
                                .iter()
                                .copied()
                                .find(|output| output.is_primary())
                        });
                    let matched = target_configuration.and_then(|target_configuration| {
                        self.current_configurations
                            .iter()
                            .copied()
                            .filter(|reference| reference.id() != target_configuration.id())
                            .find_map(|reference| {
                                super::output::scale_matching_physical_density(
                                    target_configuration,
                                    reference,
                                )
                                .map(|scale| (target_configuration.id(), scale))
                            })
                    });
                    if let Some((output, scale)) = matched {
                        self.output_layout_revision = self.output_layout_revision.saturating_add(1);
                        OutputScaleUpdate {
                            selected: &self.selected_outputs,
                            current: &mut self.current_configurations,
                            layout_revision: self.output_layout_revision,
                            desktop: &mut self.desktop,
                            server: &mut state.data.server,
                            application,
                            input: &mut self.input,
                            clients: &mut state.clients,
                        }
                        .apply(output, scale)?;
                        self.frame_state.request_composition();
                        self.presentation_schedule.request_composition_all();
                    } else {
                        warn!("physical scale matching needs two measured outputs");
                    }
                }
            }
        }
        let cursor_update = application.take_cursor_update();
        if let Some(configuration) = cursor_update.configuration {
            self.desktop.set_cursor_configuration(configuration)?;
            self.presentation_schedule.request_present_all();
            self.frame_state.request_present();
        }
        if let Some(appearance) = cursor_update.appearance {
            state.data.server.set_shell_cursor(appearance);
        }
        if let Some(active) = cursor_update.override_client {
            state.data.server.set_shell_cursor_override(active);
        }
        if let Some(vt) = application.take_virtual_terminal_switch_request() {
            self.target = SessionTarget::InactiveOwned;
            self.session
                .change_vt(vt)
                .with_context(|| format!("failed to switch to VT {vt}"))?;
        }
        state.data.server.flush_pending_resizes();
        if application.should_exit() {
            self.exit_requested = true;
        }
        Ok(true)
    }
    fn present(
        &mut self,
        state: &mut HostState<HostEvent>,
        application: &mut dyn ApplicationHost,
        frame: PolicyFrame,
    ) -> Result<bool> {
        let now = frame.now;
        // Client feedback is device/transport paced, independent of Bevy ticks.
        if let Some(image) = state.data.server.take_cursor_image(&state.clients) {
            self.desktop.set_cursor_image(image)?;
            self.presentation_schedule.request_present_all();
            self.frame_state.request_present();
        }

        let capture_ready = self.pending_capture.as_ref().is_some_and(|capture| {
            !capture.wait_for_client
                || application
                    .composition()
                    .is_some_and(|composition| composition.has_surface_frame())
        });
        let mut capture_forced_owned = false;
        if frame.work.render_composition {
            self.frame_callbacks
                .composition_rendered(state.data.server.presentation_requested());
            match composition_route(self.target, capture_ready) {
                CompositionRoute::Owned {
                    complete_callbacks,
                    capture,
                } => {
                    application
                        .composition()
                        .context("native presenter requires local composition")?
                        .render_outputs(&self.owned_request, &mut self.owned_frames)?;
                    let primary = self.selected_outputs[0].id;
                    let frame_index = primary_output_index(
                        self.owned_frames.iter().map(|frame| frame.output),
                        primary,
                    )
                    .context("owned DRM composition returned no primary frame")?;
                    let frame = &self.owned_frames[frame_index];
                    if complete_callbacks && state.data.server.presentation_requested() {
                        crate::runtime::callbacks::stage_callback_batch(
                            &mut state.callbacks,
                            &mut state.data.server,
                            [],
                        );
                        // Preserve the former owned-composition completion of
                        // older physical callbacks when a VT cannot produce
                        // vblank. Native admissions and GPU uses stay live.
                        let completed = state
                            .callbacks
                            .retire_outputs(self.selected_outputs.iter().map(|output| output.id));
                        crate::runtime::callbacks::complete_callback_batches(
                            &mut state.data.server,
                            completed,
                        );
                    }
                    if capture {
                        let capture = self
                            .pending_capture
                            .take()
                            .context("ready capture request disappeared")?;
                        let result = self
                            .desktop
                            .capture_owned(&frame.frame, &capture.path)
                            .map_err(|error| error.to_string());
                        match capture.remote_request_id {
                            Some(request_id) => application
                                .composition()
                                .context("native presenter requires local composition")?
                                .complete_capture(request_id, result),
                            None => {
                                result.map_err(anyhow::Error::msg)?;
                                return Ok(false);
                            }
                        }
                        capture_forced_owned = self.target == SessionTarget::ActivePhysical;
                    }
                }
                CompositionRoute::Physical => {
                    self.desktop.request_all_compositions();
                    self.presentation_schedule.request_composition_all();
                }
            }
        }
        let due_outputs = self.presentation_schedule.due_outputs(now);
        let physical_due = self.target == SessionTarget::ActivePhysical
            && !capture_forced_owned
            && !due_outputs.is_empty();
        if capture_forced_owned {
            self.frame_state.composition_rendered(now);
            self.frame_state.presented();
            self.frame_state.request_composition();
        } else if physical_due {
            let stage_callbacks = self.frame_callbacks.can_stage();
            let vblank_phases = due_outputs
                .iter()
                .filter_map(|output| {
                    self.output_vblank_at
                        .get(output)
                        .map(|vblank| (*output, now.saturating_duration_since(*vblank)))
                })
                .collect::<HashMap<_, _>>();
            let outcome = self.desktop.render(
                &due_outputs,
                application
                    .composition()
                    .context("DRM presenter requires local composition")?,
                &mut state.data.server,
                &mut state.callbacks,
                &vblank_phases,
                stage_callbacks,
            )?;
            let unavailable_outputs = self.desktop.unavailable_output_ids().collect::<Vec<_>>();
            self.presentation_schedule.unavailable(&unavailable_outputs);
            self.presentation_schedule.queued(&outcome.queued);
            self.presentation_schedule
                .completed_without_queue(&outcome.empty, now);
            self.presentation_schedule
                .retry_after_interval(&outcome.retry, now);
            apply_physical_outcome(
                outcome,
                frame.work,
                now,
                &mut self.frame_state,
                &mut self.target,
            );
        } else if frame.work.render_composition {
            self.frame_state.composition_rendered(now);
            self.frame_state.presented();
        } else if frame.work.advance_main {
            self.frame_state.application_advanced(now);
        }
        self.frame_callbacks
            .reconcile(state.data.server.presentation_requested());

        if frame.redraw {
            self.frame_state.request_composition();
            self.presentation_schedule.request_composition_all();
        }
        if self.pending_capture.is_none()
            && let Some(request) = application
                .composition()
                .context("native presenter requires local composition")?
                .take_capture_request()
        {
            self.pending_capture = Some(PendingCapture::remote(request.request_id, request.path));
            self.frame_state.request_composition();
        }
        if self
            .pending_capture
            .as_ref()
            .is_some_and(|capture| capture.deadline <= Instant::now())
        {
            let capture = self
                .pending_capture
                .take()
                .context("pending capture disappeared")?;
            let error = "screenshot timed out before an owned frame was available".to_owned();
            if let Some(request_id) = capture.remote_request_id {
                application
                    .composition()
                    .context("native presenter requires local composition")?
                    .complete_capture(request_id, Err(error));
            } else {
                bail!("startup {error}");
            }
        }
        Ok(true)
    }
    fn reap_before_flush(&self) -> bool {
        true
    }
    fn after_flush(&mut self, _: &mut HostState<HostEvent>) -> Result<bool> {
        Ok(!self.exit_requested)
    }
    fn shutdown(&mut self) {
        self.desktop.pause();
    }
}

struct OutputScaleUpdate<'a> {
    selected: &'a [super::output::SelectedOutput],
    current: &'a mut Vec<OutputConfiguration>,
    layout_revision: u64,
    desktop: &'a mut PhysicalDesktop,
    server: &'a mut ServerState,
    application: &'a mut dyn HostPolicy,
    input: &'a mut LibinputAdapter,
    clients: &'a mut ClientRuntime,
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
        if let Some(event) = self.input.update_output_topology(topology)
            && self.application.enqueue_input_event(event.clone())
        {
            self.clients.dispatch_unconsumed_input(event.into_runtime());
            self.server.apply_pending_client_work();
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
