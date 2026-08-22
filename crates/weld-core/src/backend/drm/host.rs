//! Standalone calloop host around Smithay's DRM output compositor.

use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use calloop::signals::Signals;
use input::Libinput;
use smithay::{
    backend::{
        drm::{
            DrmError, DrmEvent, DrmEventMetadata,
            compositor::{FrameError, FrameFlags, PrimaryPlaneElement, RenderFrameError},
            output::DrmOutputRenderElements,
        },
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::element::Element,
        session::{Event as SessionEvent, Session},
    },
    reexports::{calloop::EventLoop, wayland_server::Display},
};
use tracing::{info, warn};

use crate::{
    CompositionDemand, OutputConfiguration, OutputLayout, OutputScale, OutputTopology,
    cursor::CursorConfiguration,
    host::{
        CompositionDestination, CompositionFrame, CompositionHost, CompositionOutputRequest,
        RunOptions,
    },
    input::{RawSeatEvent, RawSeatEventKind, source::libinput::LibinputAdapter},
    runtime::{
        ChildProcesses, FrameState, HostCommandEffect, IterationWork, LoopData, PendingCapture,
        iteration_work, server_mut,
    },
    server::{ServerOptions, ServerState},
};

use super::{
    cursor::CursorState,
    device::{DrmRuntimeBootstrap, OutputManager, PhysicalOutput, SubmittedFrame},
    renderer::{CompositionElement, DrmRenderState, DrmRenderer, OutputElement},
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameAdmission {
    Idle,
    Queued {
        presentation_id: Option<u64>,
        deferred_present: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetiredFrame {
    presentation_id: Option<u64>,
    deferred_present: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PhysicalRenderOutcome {
    Queued,
    Empty,
    Inactive,
    Busy,
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
    match outcome {
        PhysicalRenderOutcome::Queued => {
            if work.render_composition {
                // This deadline remains a no-vblank fallback. Physical frame
                // retirement replaces it with the DRM vblank clock.
                frame_state.composition_rendered(now);
            }
        }
        PhysicalRenderOutcome::Empty => {
            if work.render_composition {
                frame_state.composition_rendered(now);
            }
            frame_state.presented();
        }
        PhysicalRenderOutcome::Inactive => {
            *target = SessionTarget::InactiveOwned;
            if work.advance_main {
                frame_state.application_advanced(now);
            }
            frame_state.request_composition();
        }
        PhysicalRenderOutcome::Busy if work.advance_main => {
            frame_state.application_advanced(now);
        }
        PhysicalRenderOutcome::Busy => {}
    }
}

impl FrameAdmission {
    const fn is_idle(self) -> bool {
        matches!(self, Self::Idle)
    }

    fn queue(&mut self, presentation_id: Option<u64>) {
        *self = Self::Queued {
            presentation_id,
            deferred_present: false,
        };
    }

    fn defer_present(&mut self) {
        if let Self::Queued {
            deferred_present, ..
        } = self
        {
            *deferred_present = true;
        }
    }

    fn retire(&mut self) -> Option<RetiredFrame> {
        let Self::Queued {
            presentation_id,
            deferred_present,
        } = std::mem::replace(self, Self::Idle)
        else {
            return None;
        };
        Some(RetiredFrame {
            presentation_id,
            deferred_present,
        })
    }
}

struct PhysicalPresenter {
    output_id: crate::OutputId,
    manager: OutputManager,
    output: PhysicalOutput,
    render_state: DrmRenderState,
    composition: CompositionElement,
    cursor: CursorState,
    admission: FrameAdmission,
}

impl PhysicalPresenter {
    fn new(
        mut manager: OutputManager,
        mut render_state: DrmRenderState,
        selected: &super::output::SelectedOutput,
        server: &ServerState,
    ) -> Result<Self> {
        let native_output = server
            .native_output(selected.id)
            .context("Wayland server did not install the selected DRM output")?;
        let output = {
            let mut renderer = render_state.renderer(selected.id, None);
            let render_elements: DrmOutputRenderElements<
                DrmRenderer<'_>,
                OutputElement<DrmRenderer<'_>>,
            > = DrmOutputRenderElements::default();
            manager
                .lock()
                .initialize_output(
                    selected.crtc,
                    selected.mode,
                    &[selected.connector.handle()],
                    &native_output,
                    None,
                    &mut renderer,
                    &render_elements,
                )
                .context("Smithay failed to initialize the selected DRM output")?
        };
        Ok(Self {
            output_id: selected.id,
            manager,
            output,
            render_state,
            composition: CompositionElement::new(selected.id, selected.configuration.extent())?,
            cursor: CursorState::new(
                CursorConfiguration::default(),
                selected.configuration.scale().value(),
            )?,
            admission: FrameAdmission::Idle,
        })
    }

    fn request_composition(&mut self) {
        self.composition.mark_dirty();
    }

    fn set_cursor_position(&mut self, position: crate::input::InputPosition) {
        self.cursor.set_position(position);
    }

    fn set_cursor_image(&mut self, image: crate::cursor::CursorImage) -> Result<()> {
        self.cursor.set_image(image)
    }

    fn set_cursor_configuration(&mut self, configuration: CursorConfiguration) -> Result<()> {
        self.cursor.set_configuration(configuration)
    }

    fn set_scale(&mut self, scale: OutputScale) -> Result<()> {
        self.cursor.set_scale(scale.value())
    }

    fn render(
        &mut self,
        host: &mut dyn CompositionHost,
        server: &mut ServerState,
        vblank_phase: Option<std::time::Duration>,
    ) -> Result<PhysicalRenderOutcome> {
        if !self.admission.is_idle() {
            self.admission.defer_present();
            return Ok(PhysicalRenderOutcome::Busy);
        }
        let (empty, hardware_cursor, composition_state, needs_sync) = {
            let mut renderer = self.render_state.renderer(self.output_id, Some(host));
            let cursor = self.cursor.render_element(&mut renderer)?;
            let mut elements: Vec<OutputElement<DrmRenderer<'_>>> = Vec::with_capacity(2);
            if let Some(cursor) = cursor {
                elements.push(OutputElement::from(cursor));
            }
            elements.push(OutputElement::from(self.composition.clone()));
            let result = match self.output.render_frame(
                &mut renderer,
                &elements,
                smithay::backend::renderer::Color32F::BLACK,
                FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT,
            ) {
                Ok(result) => result,
                Err(RenderFrameError::PrepareFrame(FrameError::DrmError(
                    DrmError::DeviceInactive,
                ))) => return Ok(PhysicalRenderOutcome::Inactive),
                Err(error) => {
                    return Err(anyhow!(
                        "Smithay failed to render the physical output: {error:?}"
                    ));
                }
            };
            let hardware_cursor = result.cursor_element.is_some();
            let composition_state = tracing::enabled!(
                target: "weld_drm_pacing",
                tracing::Level::TRACE
            )
            .then(|| {
                result
                    .states
                    .element_render_state(self.composition.id().clone())
            })
            .flatten();
            let needs_sync = result.needs_sync();
            if needs_sync && let PrimaryPlaneElement::Swapchain(primary) = &result.primary_element {
                primary
                    .sync
                    .wait()
                    .context("physical render synchronization was interrupted")?;
            }
            (
                result.is_empty,
                hardware_cursor,
                composition_state,
                needs_sync,
            )
        };
        let gpu_wait = self.render_state.last_gpu_wait();
        let composition_drawn = self.render_state.last_composition_drawn();
        if empty {
            if server.presentation_requested() {
                let presentation_id = server.stage_frame_callbacks();
                server.complete_frame_callbacks(presentation_id);
            }
            return Ok(PhysicalRenderOutcome::Empty);
        }
        let presentation_id = server
            .presentation_requested()
            .then(|| server.stage_frame_callbacks());
        match self.output.queue_frame(SubmittedFrame { presentation_id }) {
            Ok(()) => {
                self.admission.queue(presentation_id);
                tracing::trace!(
                    target: "weld_drm_pacing",
                    hardware_cursor,
                    composition_drawn,
                    ?composition_state,
                    needs_sync,
                    gpu_wait_micros = gpu_wait.as_micros(),
                    vblank_phase_micros = vblank_phase.map(|phase| phase.as_micros()),
                    "queued physical frame"
                );
                Ok(PhysicalRenderOutcome::Queued)
            }
            Err(FrameError::EmptyFrame) => {
                if let Some(presentation_id) = presentation_id {
                    server.complete_frame_callbacks(presentation_id);
                }
                Ok(PhysicalRenderOutcome::Empty)
            }
            Err(FrameError::DrmError(DrmError::DeviceInactive)) => {
                if let Some(presentation_id) = presentation_id {
                    server.complete_frame_callbacks(presentation_id);
                }
                Ok(PhysicalRenderOutcome::Inactive)
            }
            Err(error) => Err(error).context("Smithay failed to queue the physical output"),
        }
    }

    fn retire(&mut self, server: &mut ServerState) -> Result<Option<bool>> {
        let submitted = self
            .output
            .frame_submitted()
            .context("Smithay failed to retire the physical output frame")?;
        let Some(submitted) = submitted else {
            return Ok(None);
        };
        let admission = self.admission.retire().unwrap_or(RetiredFrame {
            presentation_id: None,
            deferred_present: false,
        });
        if submitted.presentation_id != admission.presentation_id {
            warn!(
                queued = ?admission.presentation_id,
                submitted = ?submitted.presentation_id,
                "physical frame callback identity diverged"
            );
        }
        if let Some(presentation_id) = submitted.presentation_id.or(admission.presentation_id) {
            server.complete_frame_callbacks(presentation_id);
        }
        Ok(Some(admission.deferred_present))
    }

    fn pause(&mut self) {
        self.manager.pause();
    }

    fn activate(&mut self, server: &mut ServerState) -> Result<()> {
        self.manager
            .lock()
            .activate(true)
            .context("failed to reactivate the Smithay DRM output manager")?;
        if let Some(presentation_id) = self
            .admission
            .retire()
            .and_then(|frame| frame.presentation_id)
        {
            server.complete_frame_callbacks(presentation_id);
        }
        self.request_composition();
        Ok(())
    }

    fn capture_owned(&self, frame: &CompositionFrame, path: &std::path::Path) -> Result<()> {
        crate::renderer::capture_owned_frame(
            self.render_state.device(),
            self.render_state.queue(),
            frame,
            path,
        )
    }
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
        selected_output,
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
            outputs: vec![selected_output.definition.clone()],
            dmabuf_capabilities: dmabuf_capabilities.as_ref(),
            dmabuf_sources,
        },
    )?;
    let mut loop_data = LoopData::new(server);
    let mut presenter = PhysicalPresenter::new(
        output_manager,
        render_state,
        &selected_output,
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

    let topology = OutputTopology::new(OutputLayout::new(1, vec![selected_output.configuration])?);
    let mut input = LibinputAdapter::new(topology);
    let initial_input = input.initial_event();
    let _ = application.enqueue_input_event(initial_input.clone());
    loop_data.server.forward_raw_input(initial_input);
    presenter.set_cursor_position(input.pointer_position());

    let mut children = ChildProcesses::default();
    let child_requested = children.spawn_requested(&loop_data.server, &options.client)?;
    let mut pending_capture = options
        .screenshot
        .map(|path| PendingCapture::startup(path, child_requested));
    let mut frame_state = FrameState::with_interval(selected_output.frame_interval);
    let mut current_configuration = selected_output.configuration;
    let mut output_layout_revision = 1_u64;
    let mut target = if session.is_active() {
        SessionTarget::ActivePhysical
    } else {
        SessionTarget::InactiveOwned
    };
    let owned_request = [CompositionOutputRequest {
        output: selected_output.id,
        destination: CompositionDestination::Owned,
    }];
    let mut owned_frames = Vec::with_capacity(1);
    let mut input_pending = false;
    let mut next_input_update = Instant::now();
    let mut next_remote_service = Instant::now();
    let mut last_vblank_at = None;
    let mut last_vblank_sequence = None;
    let mut exit_requested = false;

    info!(
        socket = ?loop_data.server.socket_name,
        output = %selected_output.head.name(),
        "Weld DRM compositor is ready"
    );
    while !exit_requested {
        let now = Instant::now();
        if input_pending && now >= next_input_update {
            frame_state.request_update();
        }
        calloop
            .dispatch(Some(frame_state.composition_timeout(now)), &mut loop_data)
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
                    presenter.pause();
                }
                HostEvent::Session(SessionEvent::ActivateSession) => {
                    libinput_context
                        .resume()
                        .map_err(|_| anyhow!("failed to resume libinput after VT activation"))?;
                    presenter.activate(&mut loop_data.server)?;
                    target = SessionTarget::ActivePhysical;
                    frame_state.request_composition();
                }
                HostEvent::Drm {
                    event: DrmEvent::VBlank(crtc),
                    metadata,
                } if crtc == selected_output.crtc => {
                    let vblank_at = Instant::now();
                    let sequence = metadata.map(|event| event.sequence);
                    let sequence_delta = sequence
                        .zip(last_vblank_sequence)
                        .map(|(current, previous)| current.wrapping_sub(previous));
                    let wall_interval = last_vblank_at
                        .map(|previous| vblank_at.saturating_duration_since(previous));
                    let retired = presenter.retire(&mut loop_data.server)?;
                    if let Some(deferred_present) = retired {
                        match target {
                            SessionTarget::ActivePhysical => {
                                frame_state.physical_frame_retired();
                                if input_pending {
                                    frame_state.request_update();
                                    next_input_update = vblank_at + selected_output.frame_interval;
                                }
                            }
                            SessionTarget::InactiveOwned => frame_state.presented(),
                        }
                        if deferred_present {
                            frame_state.request_present();
                        }
                    }
                    tracing::trace!(
                        target: "weld_drm_pacing",
                        ?sequence,
                        ?sequence_delta,
                        wall_interval_micros = ?wall_interval.map(|interval| interval.as_micros()),
                        deferred_present = ?retired,
                        "retired physical frame"
                    );
                    last_vblank_at = Some(vblank_at);
                    if sequence.is_some() {
                        last_vblank_sequence = sequence;
                    }
                }
                HostEvent::Drm {
                    event: DrmEvent::VBlank(_),
                    ..
                } => {}
                HostEvent::Drm {
                    event: DrmEvent::Error(error),
                    ..
                } => return Err(error).context("Smithay DRM notifier failed"),
                HostEvent::Input(event) => {
                    for event in input.convert(event).into_iter().flatten() {
                        if matches!(event.event, RawSeatEventKind::PointerMotion { .. }) {
                            presenter.set_cursor_position(input.pointer_position());
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
                match application.enqueue_surface_event(event) {
                    CompositionDemand::Ordinary => frame_state.request_composition(),
                    CompositionDemand::Settle => frame_state.request_settled_composition(),
                }
            }
        }
        if loop_data.server.presentation_requested() {
            match target {
                SessionTarget::ActivePhysical => frame_state.request_present(),
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
                    .map(|vblank| vblank + selected_output.frame_interval)
                    .unwrap_or(now + selected_output.frame_interval),
                SessionTarget::InactiveOwned => now + selected_output.frame_interval,
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
                        if let Some(scale) = current_configuration.scale().adjust(adjustment) {
                            output_layout_revision = output_layout_revision.saturating_add(1);
                            OutputScaleUpdate {
                                selected: &selected_output,
                                current: &mut current_configuration,
                                layout_revision: output_layout_revision,
                                presenter: &mut presenter,
                                server: &mut loop_data.server,
                                application: application.as_mut(),
                                input: &mut input,
                            }
                            .apply(scale)?;
                            frame_state.request_composition();
                        }
                    }
                    HostCommandEffect::MatchOutputPhysicalScale => {
                        warn!("physical scale matching needs another measured output");
                    }
                }
            }
            let cursor_update = application.take_cursor_update();
            if let Some(configuration) = cursor_update.configuration {
                presenter.set_cursor_configuration(configuration)?;
                frame_state.request_present();
            }
            if let Some(appearance) = cursor_update.appearance {
                loop_data.server.set_shell_cursor(appearance);
            }
            if let Some(image) = loop_data.server.take_cursor_image() {
                presenter.set_cursor_image(image)?;
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
            match composition_route(target, capture_ready) {
                CompositionRoute::Owned {
                    complete_callbacks,
                    capture,
                } => {
                    application.render_outputs(&owned_request, &mut owned_frames)?;
                    let frame = owned_frames
                        .pop()
                        .context("owned DRM composition returned no frame")?;
                    if complete_callbacks && loop_data.server.presentation_requested() {
                        let presentation_id = loop_data.server.stage_frame_callbacks();
                        loop_data.server.complete_frame_callbacks(presentation_id);
                    }
                    if capture {
                        let capture = pending_capture
                            .take()
                            .context("ready capture request disappeared")?;
                        let result = presenter
                            .capture_owned(&frame.frame, &capture.path)
                            .map_err(|error| error.to_string());
                        match capture.remote_request_id {
                            Some(request_id) => application.complete_capture(request_id, result),
                            None => {
                                result.map_err(anyhow::Error::msg)?;
                                presenter.pause();
                                return Ok(());
                            }
                        }
                        capture_forced_owned = target == SessionTarget::ActivePhysical;
                    }
                }
                CompositionRoute::Physical => presenter.request_composition(),
            }
        }
        let physical_due = target == SessionTarget::ActivePhysical
            && !capture_forced_owned
            && (work.render_composition || frame_state.presentation_due());
        if capture_forced_owned {
            frame_state.composition_rendered(now);
            frame_state.presented();
            frame_state.request_composition();
        } else if physical_due {
            let vblank_phase = last_vblank_at.map(|vblank| now.saturating_duration_since(vblank));
            let outcome =
                presenter.render(application.as_mut(), &mut loop_data.server, vblank_phase)?;
            apply_physical_outcome(outcome, work, now, &mut frame_state, &mut target);
        } else if work.render_composition {
            frame_state.composition_rendered(now);
            frame_state.presented();
        } else if work.advance_main {
            frame_state.application_advanced(now);
        }

        if bevy_requested_redraw {
            frame_state.request_composition();
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
    presenter.pause();
    Ok(())
}

struct OutputScaleUpdate<'a> {
    selected: &'a super::output::SelectedOutput,
    current: &'a mut OutputConfiguration,
    layout_revision: u64,
    presenter: &'a mut PhysicalPresenter,
    server: &'a mut ServerState,
    application: &'a mut dyn CompositionHost,
    input: &'a mut LibinputAdapter,
}

impl OutputScaleUpdate<'_> {
    fn apply(self, scale: OutputScale) -> Result<()> {
        let configuration = OutputConfiguration::new(
            self.selected.id,
            self.current.extent(),
            scale,
            self.current.position(),
            true,
            self.selected.head.physical_size(),
        )?;
        let metrics = super::output::metrics_for_configuration(configuration, self.selected.mode)?;
        self.server.update_output_metrics(metrics);
        self.application.update_output_topology(&[configuration]);
        let topology = OutputTopology::new(OutputLayout::new(
            self.layout_revision,
            vec![configuration],
        )?);
        if let Some(event) = self.input.update_output_topology(topology) {
            let _ = self.application.enqueue_input_event(event.clone());
            self.server.forward_raw_input(event);
        }
        self.presenter
            .set_cursor_position(self.input.pointer_position());
        *self.current = configuration;
        self.presenter.set_scale(scale)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::runtime::{FrameState, IterationWork};

    use super::{
        CompositionRoute, FrameAdmission, PhysicalRenderOutcome, SessionTarget,
        apply_physical_outcome, composition_route,
    };

    #[test]
    fn queued_admission_is_released_exactly_once() {
        let mut admission = FrameAdmission::Idle;
        admission.queue(Some(7));
        admission.defer_present();
        let retired = admission.retire().expect("queued frame should retire");
        assert_eq!(retired.presentation_id, Some(7));
        assert!(retired.deferred_present);
        assert_eq!(admission.retire(), None);
    }

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
    fn busy_physical_output_paces_main_without_discarding_composition() {
        let mut frame_state = FrameState::with_interval(Duration::from_millis(16));
        let mut target = SessionTarget::ActivePhysical;
        apply_physical_outcome(
            PhysicalRenderOutcome::Busy,
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
            PhysicalRenderOutcome::Inactive,
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
