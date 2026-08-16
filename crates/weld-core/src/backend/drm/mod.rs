//! Standalone libseat, udev, libinput, and Smithay GBM/KMS backend.

use std::{
    collections::{BTreeSet, HashMap},
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
        drm::{DrmDevice, DrmEvent, DrmSurface},
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
        CompositionDemand, CompositionDestination, CompositionHost, CompositionOutputRequest,
        PreparedHost, RenderContext, RunOptions,
    },
    input::{
        InputPosition, RawPointerUpdate, RawSeatEvent, RawSeatEventKind,
        source::libinput::LibinputAdapter,
    },
    output::{
        OutputConfiguration, OutputFootprintProvenance, OutputHead, OutputId, OutputLayout,
        OutputTopology,
    },
    renderer::{CursorOverlay, GpuCursor, read_composition_rgba, write_png},
    runtime::{
        ChildProcesses, FRAME_INTERVAL, FrameState, HostCommand, HostCommandEffect, LoopData,
        PendingCapture, REMOTE_DEBUG_MAINTENANCE_INTERVAL, iteration_work, server_mut,
    },
    server::{OutputMetrics, ServerOptions, ServerOutputDefinition, ServerState},
};

mod discovery;
mod gpu;
mod presenter;

use discovery::{DrmDeviceDiscovery, connector_name, discover_outputs, output_description};
use gpu::DrmGpu;
use presenter::{
    AcquiredFrame, PresenterEvent, PresenterHandle, PresenterTargetAvailability, PresenterWorker,
};

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
    outputs: HashMap<connector::Handle, MonitoredOutput>,
    event_source_healthy: bool,
}

struct MonitoredOutput {
    id: OutputId,
    connected: bool,
    mode_compatible: bool,
}

struct Presenters {
    handles: HashMap<OutputId, PresenterHandle>,
}

struct PreparedDrmOutput {
    id: OutputId,
    connector: connector::Handle,
    crtc: smithay::reexports::drm::control::crtc::Handle,
    metrics: OutputMetrics,
    refresh_millihertz: u32,
    surface: DrmSurface,
}

impl Presenters {
    fn presentable_outputs(&self, connected: &BTreeSet<OutputId>) -> BTreeSet<OutputId> {
        select_presentable_outputs(connected, |output| {
            self.handles
                .get(&output)
                .map(PresenterHandle::target_availability)
        })
    }

    fn target_availability(&self, outputs: &BTreeSet<OutputId>) -> PresenterTargetAvailability {
        if outputs.is_empty() {
            return PresenterTargetAvailability::Unavailable;
        }
        batch_target_availability(
            outputs
                .iter()
                .filter_map(|output| self.handles.get(output))
                .map(PresenterHandle::target_availability),
        )
    }

    fn acquire_frames(
        &mut self,
        outputs: &BTreeSet<OutputId>,
    ) -> Option<Vec<(OutputId, AcquiredFrame)>> {
        if self.target_availability(outputs) != PresenterTargetAvailability::Ready {
            return None;
        }
        let mut frames = Vec::with_capacity(outputs.len());
        for output in outputs.iter().copied() {
            let frame = self.handles.get_mut(&output)?.acquire_frame();
            let Some(frame) = frame else {
                for (acquired_output, acquired) in frames {
                    if let Some(presenter) = self.handles.get_mut(&acquired_output) {
                        presenter.abort_frame(acquired);
                    }
                }
                return None;
            };
            frames.push((output, frame));
        }
        Some(frames)
    }

    fn suspend(&mut self) {
        for presenter in self.handles.values_mut() {
            presenter.suspend();
        }
    }

    fn activate_after_session(&mut self, outputs: &BTreeSet<OutputId>) {
        for output in outputs {
            let Some(presenter) = self.handles.get_mut(output) else {
                continue;
            };
            if let Err(error) = presenter.activate_after_session() {
                error!(?output, %error, "failed to activate output after session recovery");
            }
        }
    }

    fn handle_event(&mut self, event: &PresenterEvent) {
        for presenter in self.handles.values_mut() {
            presenter.handle_event(event);
        }
    }

    fn frame_submitted(&mut self, crtc: smithay::reexports::drm::control::crtc::Handle) {
        for presenter in self.handles.values_mut() {
            presenter.frame_submitted(crtc);
        }
    }

    fn finish_frames(
        &mut self,
        frames: Vec<(OutputId, AcquiredFrame)>,
        cursors: &HashMap<OutputId, CursorOverlay>,
    ) -> Result<()> {
        let mut frames = frames.into_iter();
        while let Some((output, frame)) = frames.next() {
            let Some(presenter) = self.handles.get_mut(&output) else {
                self.abort_frames(frames.collect());
                anyhow::bail!("presenter for output {output:?} disappeared");
            };
            let cursor = cursors
                .get(&output)
                .cloned()
                .unwrap_or_else(CursorOverlay::hidden);
            if let Err(error) = presenter.finish_frame(&frame, &cursor) {
                presenter.abort_frame(frame);
                self.abort_frames(frames.collect());
                return Err(error).with_context(|| format!("failed to finalize output {output:?}"));
            }
        }
        Ok(())
    }

    fn abort_frames(&mut self, frames: Vec<(OutputId, AcquiredFrame)>) {
        for (output, frame) in frames {
            if let Some(presenter) = self.handles.get_mut(&output) {
                presenter.abort_frame(frame);
            }
        }
    }

    fn stop(&mut self) {
        for presenter in self.handles.values_mut() {
            presenter.stop();
        }
    }
}

fn select_presentable_outputs(
    connected: &BTreeSet<OutputId>,
    mut availability: impl FnMut(OutputId) -> Option<PresenterTargetAvailability>,
) -> BTreeSet<OutputId> {
    connected
        .iter()
        .copied()
        .filter(|output| {
            availability(*output).is_some_and(|availability| {
                availability != PresenterTargetAvailability::Unavailable
            })
        })
        .collect()
}

fn batch_target_availability(
    availability: impl IntoIterator<Item = PresenterTargetAvailability>,
) -> PresenterTargetAvailability {
    let mut batch = PresenterTargetAvailability::Unavailable;
    for target in availability {
        match target {
            PresenterTargetAvailability::Unavailable => {
                continue;
            }
            PresenterTargetAvailability::Busy => batch = PresenterTargetAvailability::Busy,
            PresenterTargetAvailability::Ready
                if batch == PresenterTargetAvailability::Unavailable =>
            {
                batch = PresenterTargetAvailability::Ready;
            }
            PresenterTargetAvailability::Ready => {}
        }
    }
    batch
}

struct DrmHostCommandContext<'a> {
    children: &'a mut ChildProcesses,
    server: &'a mut ServerState,
    input_adapter: &'a mut LibinputAdapter,
    events: &'a mut std::collections::VecDeque<DrmRuntimeEvent>,
    shell: &'a mut dyn CompositionHost,
    output_metrics: &'a mut OutputMetrics,
    output_configurations: &'a mut Vec<OutputConfiguration>,
    output_layout_revision: &'a mut u64,
    cursor: &'a mut DrmCursor,
    frame_state: &'a mut FrameState,
    output_id: OutputId,
}

struct DrmCursor {
    gpus: HashMap<OutputId, GpuCursor>,
    overlays: HashMap<OutputId, CursorOverlay>,
    outputs: Vec<OutputConfiguration>,
    animation_deadline: Option<Instant>,
    position: Option<InputPosition>,
}

impl DrmCursor {
    fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        outputs: Vec<OutputConfiguration>,
        now: Instant,
    ) -> Self {
        let gpus = outputs
            .iter()
            .map(|output| {
                (
                    output.id(),
                    GpuCursor::new(
                        device,
                        queue,
                        crate::cursor::CursorConfiguration::default(),
                        output.scale().value(),
                        now,
                    ),
                )
            })
            .collect();
        let overlays = outputs
            .iter()
            .map(|output| (output.id(), CursorOverlay::hidden()))
            .collect();
        Self {
            gpus,
            overlays,
            outputs,
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

    fn update_outputs(&mut self, outputs: &[OutputConfiguration]) {
        self.outputs = outputs.to_vec();
        for output in outputs {
            if let Some(gpu) = self.gpus.get_mut(&output.id()) {
                gpu.set_output_scale(output.scale().value());
            }
        }
    }

    fn apply_host_update(
        &mut self,
        server: &mut ServerState,
        update: CursorHostUpdate,
        now: Instant,
    ) {
        if let Some(configuration) = update.configuration {
            for gpu in self.gpus.values_mut() {
                gpu.set_configuration(configuration.clone(), now);
            }
        }
        if let Some(appearance) = update.appearance {
            server.set_shell_cursor(appearance);
        }
    }

    fn refresh(&mut self, server: &mut ServerState, frame_state: &mut FrameState, now: Instant) {
        if let Some(image) = server.take_cursor_image() {
            for gpu in self.gpus.values_mut() {
                gpu.set_image(image.clone(), now);
            }
        }
        self.animation_deadline = None;
        for output in &self.outputs {
            let Some(gpu) = self.gpus.get_mut(&output.id()) else {
                continue;
            };
            let position = self.position.map(|position| InputPosition {
                x: position.x - f64::from(output.position().x),
                y: position.y - f64::from(output.position().y),
            });
            gpu.set_position(position);
            gpu.set_output_scale(output.scale().value());
            let evaluated = gpu.evaluate(now);
            self.animation_deadline = match (self.animation_deadline, evaluated.next_animation) {
                (Some(current), Some(next)) => Some(current.min(next)),
                (None, next) => next,
                (current, None) => current,
            };
            let overlay = self
                .overlays
                .entry(output.id())
                .or_insert_with(CursorOverlay::hidden);
            if evaluated.overlay != *overlay {
                *overlay = evaluated.overlay;
                frame_state.request_composition();
            }
        }
    }
}

fn apply_host_command(
    mut context: DrmHostCommandContext<'_>,
    command: HostCommand,
) -> Result<bool> {
    match context.children.apply(context.server, command)? {
        HostCommandEffect::Continue => Ok(false),
        HostCommandEffect::Exit => Ok(true),
        HostCommandEffect::AdjustOutputScale(adjustment) => {
            let current = match OutputScale::new(context.output_metrics.scale_factor()) {
                Ok(current) => current,
                Err(error) => {
                    warn!(%error, "ignored output-scale adjustment from invalid current state");
                    return Ok(false);
                }
            };
            let Some(next_scale) = current.adjust(adjustment) else {
                return Ok(false);
            };
            apply_primary_output_scale(&mut context, next_scale)?;
            Ok(false)
        }
        HostCommandEffect::MatchOutputPhysicalScale => {
            let next_scale =
                match matched_primary_scale(context.output_configurations, context.output_id) {
                    Ok(scale) => scale,
                    Err(error) => {
                        warn!(%error, "ignored physical output-scale match");
                        return Ok(false);
                    }
                };
            apply_primary_output_scale(&mut context, next_scale)?;
            Ok(false)
        }
    }
}

const MINIMUM_MATCHED_OUTPUT_SCALE: f64 = 0.5;
const MAXIMUM_MATCHED_OUTPUT_SCALE: f64 = 4.0;

fn matched_primary_scale(
    outputs: &[OutputConfiguration],
    primary_id: OutputId,
) -> Result<OutputScale> {
    let primary = outputs
        .iter()
        .copied()
        .find(|output| output.id() == primary_id)
        .context("primary output is unavailable")?;
    let reference = outputs
        .iter()
        .copied()
        .find(|output| {
            !output.is_primary()
                && output.footprint().provenance() == OutputFootprintProvenance::Measured
        })
        .context("no measured non-primary output is available as a scale reference")?;
    let primary_dpi = measured_diagonal_dpi(primary)?;
    let reference_dpi = measured_diagonal_dpi(reference)?;
    let value = primary_dpi / reference_dpi * reference.scale().value();
    if !(MINIMUM_MATCHED_OUTPUT_SCALE..=MAXIMUM_MATCHED_OUTPUT_SCALE).contains(&value) {
        anyhow::bail!(
            "matched scale {value:.3} is outside the safe range {MINIMUM_MATCHED_OUTPUT_SCALE:.1}..={MAXIMUM_MATCHED_OUTPUT_SCALE:.1}"
        );
    }
    OutputScale::new(value)
}

fn measured_diagonal_dpi(output: OutputConfiguration) -> Result<f64> {
    let footprint = output.footprint();
    if footprint.provenance() != OutputFootprintProvenance::Measured {
        anyhow::bail!(
            "output {:?} has no measured physical dimensions",
            output.id()
        );
    }
    let extent = output.extent();
    let pixel_diagonal = f64::from(extent.width).hypot(f64::from(extent.height));
    let inch_diagonal = footprint
        .width_millimeters()
        .hypot(footprint.height_millimeters())
        / 25.4;
    let dpi = pixel_diagonal / inch_diagonal;
    if !dpi.is_finite() || dpi <= 0.0 {
        anyhow::bail!("output {:?} produced invalid diagonal DPI", output.id());
    }
    Ok(dpi)
}

fn apply_primary_output_scale(
    context: &mut DrmHostCommandContext<'_>,
    next_scale: OutputScale,
) -> Result<()> {
    let candidate = match context.output_metrics.with_scale(next_scale) {
        Ok(candidate) => candidate,
        Err(error) => {
            warn!(
                scale_factor = next_scale.value(),
                %error,
                "ignored invalid runtime output scale"
            );
            return Ok(());
        }
    };
    let mut candidate_configurations = context.output_configurations.clone();
    let current_configuration = candidate_configurations
        .iter_mut()
        .find(|output| output.id() == context.output_id)
        .context("primary output disappeared during scale adjustment")?;
    if current_configuration.scale() == next_scale {
        return Ok(());
    }
    let Ok(configuration) = current_configuration.with_scale(next_scale) else {
        warn!("ignored an output scale that invalidated its configuration");
        return Ok(());
    };
    *current_configuration = configuration;
    let candidate_configurations = match center_primary_below_others(candidate_configurations) {
        Ok(configurations) => configurations,
        Err(error) => {
            warn!(%error, "ignored an output scale that could not be placed");
            return Ok(());
        }
    };
    let Some(candidate_revision) = context.output_layout_revision.checked_add(1) else {
        warn!("ignored an output-scale change because the layout revision is exhausted");
        return Ok(());
    };
    let candidate_layout =
        match OutputLayout::new(candidate_revision, candidate_configurations.clone()) {
            Ok(layout) => layout,
            Err(error) => {
                warn!(%error, "ignored an output scale that invalidated the layout");
                return Ok(());
            }
        };
    let topology = OutputTopology::new(candidate_layout);
    let output_positions = candidate_configurations
        .iter()
        .map(|configuration| {
            logical_position(configuration.position())
                .map(|position| (configuration.id(), position))
        })
        .collect::<Result<Vec<_>>>();
    let output_positions = match output_positions {
        Ok(positions) => positions,
        Err(error) => {
            warn!(%error, "ignored an output scale with invalid protocol positions");
            return Ok(());
        }
    };

    context.server.update_output_metrics(candidate);
    context.server.update_output_positions(&output_positions);
    if let Some(event) = context.input_adapter.update_output_topology(topology) {
        context.events.push_back(DrmRuntimeEvent::Input(event));
    }
    context
        .shell
        .update_output_topology(&candidate_configurations);
    context.cursor.update_outputs(&candidate_configurations);
    *context.output_configurations = candidate_configurations;
    *context.output_layout_revision = candidate_revision;
    *context.output_metrics = candidate;
    context.frame_state.request_settled_composition();
    info!(
        scale_factor = candidate.scale_factor(),
        "updated standalone output scale"
    );
    Ok(())
}

impl OutputMonitor {
    fn physical_outputs(&self, session_active: bool) -> BTreeSet<OutputId> {
        physical_output_ids(
            session_active,
            self.event_source_healthy,
            self.outputs.values(),
        )
    }

    fn physical_available(&self, session_active: bool) -> bool {
        !self.physical_outputs(session_active).is_empty()
    }

    fn handle(
        &mut self,
        event: UdevEvent,
        drm: &mut DrmDevice,
        presenters: &mut Presenters,
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
                        DrmScanEvent::Disconnected { connector, .. } => {
                            let Some(output) = self.outputs.get_mut(&connector.handle()) else {
                                continue;
                            };
                            output.connected = false;
                            if let Some(presenter) = presenters.handles.get_mut(&output.id) {
                                presenter.suspend();
                            }
                            warn!(output = ?output.id, "DRM connector disconnected; composition remains live");
                        }
                        DrmScanEvent::Connected { connector, .. } => {
                            let Some(output) = self.outputs.get_mut(&connector.handle()) else {
                                warn!(connector = %connector_name(&connector), "new DRM output requires restarting Weld");
                                continue;
                            };
                            output.connected = true;
                            let mode_compatible = output.mode_compatible;
                            let output_id = output.id;
                            if session_active && self.event_source_healthy && mode_compatible {
                                if let Err(error) = drm.reset_state() {
                                    error!(%error, "failed to reset DRM state after connector recovery");
                                } else {
                                    presenters.activate_after_session(&BTreeSet::from([output_id]));
                                    info!(
                                        output = ?output_id,
                                        "active DRM connector reconnected; GBM/KMS presentation restored"
                                    );
                                }
                            } else if !mode_compatible {
                                warn!(
                                    "active DRM connector reconnected with changed modes; restart is still required"
                                );
                            }
                        }
                        DrmScanEvent::Changed { connector, .. } => {
                            let Some(output) = self.outputs.get_mut(&connector.handle()) else {
                                continue;
                            };
                            output.connected = false;
                            output.mode_compatible = false;
                            if let Some(presenter) = presenters.handles.get_mut(&output.id) {
                                presenter.suspend();
                            }
                            warn!(
                                output = ?output.id,
                                "active connector modes changed; physical presentation is unavailable until restart"
                            );
                        }
                    }
                }
            }
            UdevEvent::Removed { device_id } if device_id == self.device_id => {
                for output in self.outputs.values_mut() {
                    output.connected = false;
                }
                presenters.suspend();
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

fn physical_output_ids<'a>(
    session_active: bool,
    event_source_healthy: bool,
    outputs: impl IntoIterator<Item = &'a MonitoredOutput>,
) -> BTreeSet<OutputId> {
    if !session_active || !event_source_healthy {
        return BTreeSet::new();
    }
    outputs
        .into_iter()
        .filter(|output| output.connected && output.mode_compatible)
        .map(|output| output.id)
        .collect()
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
    let discovered = discover_outputs(&device.drm)?;
    let scanner = discovered.scanner;
    let discovered_outputs = discovered.outputs;
    let primary_output_id = discovered_outputs
        .first()
        .map(|output| output.id)
        .context("DRM discovery returned no usable output")?;
    let (mut drm_device, drm_notifier) = DrmDevice::new(device.drm.clone(), true)
        .context("failed to initialize Smithay DRM device")?;
    let gpu = DrmGpu::new(device.device_id, &device.device_path)?;
    let mut described_outputs = Vec::with_capacity(discovered_outputs.len());
    for output in discovered_outputs {
        let scale = if output.id == primary_output_id {
            options.output_scale
        } else {
            OutputScale::default()
        };
        let (descriptor, head, metrics) =
            output_description(output.id, &output.connector, output.mode, scale)?;
        let configuration = OutputConfiguration::new(
            output.id,
            crate::surface::Extent::new(metrics.physical_width(), metrics.physical_height()),
            OutputScale::new(metrics.scale_factor())?,
            crate::surface::LogicalPoint::ZERO,
            output.id == primary_output_id,
            head.physical_size(),
        )?;
        described_outputs.push((output, descriptor, head, metrics, configuration));
    }
    let configurations = center_primary_below_others(
        described_outputs
            .iter()
            .map(|(_, _, _, _, configuration)| *configuration)
            .collect(),
    )?;
    let output_heads = described_outputs
        .iter()
        .map(|(_, _, head, _, _)| head.clone())
        .collect::<Vec<OutputHead>>();
    let mut server_outputs = Vec::with_capacity(described_outputs.len());
    let mut prepared_outputs = Vec::with_capacity(described_outputs.len());
    for (output, descriptor, _, metrics, _) in described_outputs {
        let configuration = configurations
            .iter()
            .copied()
            .find(|configuration| configuration.id() == output.id)
            .context("centered DRM layout omitted a discovered output")?;
        let surface = drm_device
            .create_surface(output.crtc, output.mode, &[output.connector.handle()])
            .with_context(|| {
                format!(
                    "failed to create Smithay DRM surface for {}",
                    connector_name(&output.connector)
                )
            })?;
        let refresh_millihertz = u32::try_from(smithay::output::Mode::from(output.mode).refresh)
            .context("DRM mode has a negative refresh rate")?;
        server_outputs.push(ServerOutputDefinition {
            id: output.id,
            descriptor,
            metrics,
            logical_position: logical_position(configuration.position())?,
            primary: output.id == primary_output_id,
        });
        prepared_outputs.push(PreparedDrmOutput {
            id: output.id,
            connector: output.connector.handle(),
            crtc: output.crtc,
            metrics,
            refresh_millihertz,
            surface,
        });
    }
    let topology = OutputTopology::new(OutputLayout::new(1, configurations.clone())?);
    let input_adapter = LibinputAdapter::new(topology);
    let mut output_metrics = prepared_outputs
        .iter()
        .find(|output| output.id == primary_output_id)
        .map(|output| output.metrics)
        .context("primary DRM output lost its metrics")?;
    let refresh_millihertz = prepared_outputs
        .iter()
        .map(|output| output.refresh_millihertz)
        .min()
        .context("DRM output set has no refresh rate")?;
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
            outputs: server_outputs,
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

    let connector_names = prepared_outputs
        .iter()
        .map(|output| format!("{:?}", output.connector))
        .collect::<Vec<_>>()
        .join(", ");
    let context = RenderContext {
        instance: gpu.instance.clone(),
        adapter: gpu.adapter.clone(),
        device: gpu.device.clone(),
        queue: gpu.queue.clone(),
        dmabuf: crate::dmabuf::DmabufContext::new(dmabuf_release_sender, dmabuf_sources),
        output_heads,
        outputs: configurations.clone(),
        composition_format: wgpu::TextureFormat::Bgra8UnormSrgb,
    };
    let DrmDeviceDiscovery {
        session,
        drm,
        device_id,
        device_path,
    } = device;

    Ok(PreparedHost::new(context, move |host| {
        let mut shell = host;
        let mut output_configurations = configurations;
        let mut output_layout_revision = 1_u64;
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
        let mut presenter_worker = PresenterWorker::spawn(&gpu, presenter_events)?;
        let mut presenter_handles = HashMap::with_capacity(prepared_outputs.len());
        let mut monitored_outputs = HashMap::with_capacity(prepared_outputs.len());
        for output in prepared_outputs {
            let presenter = PresenterHandle::new(
                &gpu,
                output.surface,
                drm.clone(),
                output.id,
                output.crtc,
                &presenter_worker,
            )?;
            presenter_handles.insert(output.id, presenter);
            monitored_outputs.insert(
                output.connector,
                MonitoredOutput {
                    id: output.id,
                    connected: true,
                    mode_compatible: true,
                },
            );
        }
        let mut presenters = Presenters {
            handles: presenter_handles,
        };
        let mut output_monitor = OutputMonitor {
            scanner,
            device_id,
            device_path,
            outputs: monitored_outputs,
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
            output_configurations.clone(),
            Instant::now(),
        );
        let mut session_active = true;
        info!(
            socket = ?loop_data.server.socket_name,
            connectors = %connector_names,
            "Weld GBM/KMS compositor is ready"
        );
        let mut exit_requested = false;
        while !exit_requested {
            let now = Instant::now();
            let connected_outputs = output_monitor.physical_outputs(session_active);
            let presentable_outputs = presenters.presentable_outputs(&connected_outputs);
            let physical_output_available = !presentable_outputs.is_empty();
            let capture_ready = pending_capture
                .as_ref()
                .is_some_and(|capture| !capture.wait_for_client || shell.has_surface_frame());
            let composition_blocked = physical_output_available
                && !capture_ready
                && presenters.target_availability(&presentable_outputs)
                    == PresenterTargetAvailability::Busy;
            let timeout = dispatch_timeout(DispatchTimeoutContext {
                frame_state: &frame_state,
                capture: pending_capture.as_ref(),
                remote_debug_enabled,
                physical_output_available,
                composition_blocked,
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
                            presenters.suspend();
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
                                        presenters.activate_after_session(
                                            &output_monitor.physical_outputs(session_active),
                                        );
                                        frame_state.request_composition();
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
                                &mut presenters,
                                session_active,
                            );
                            if output_monitor.physical_available(session_active) {
                                frame_state.request_composition();
                            }
                        }
                        DrmRuntimeEvent::Drm(DrmEvent::VBlank(crtc)) => {
                            event_counts[3] += 1;
                            presenters.frame_submitted(crtc);
                        }
                        DrmRuntimeEvent::Drm(DrmEvent::Error(error)) => {
                            event_counts[3] += 1;
                            output_monitor.event_source_healthy = false;
                            presenters.suspend();
                            drm_device.pause();
                            error!(%error, "DRM page-flip event source failed; physical output requires restart");
                        }
                        DrmRuntimeEvent::Presenter(event) => {
                            event_counts[4] += 1;
                            log_presenter_event(&event);
                            let connected_outputs = output_monitor.physical_outputs(session_active);
                            let presentable_outputs =
                                presenters.presentable_outputs(&connected_outputs);
                            let availability = presenters.target_availability(&presentable_outputs);
                            presenters.handle_event(&event);
                            let presentable_outputs =
                                presenters.presentable_outputs(&connected_outputs);
                            if availability != PresenterTargetAvailability::Ready
                                && presenters.target_availability(&presentable_outputs)
                                    == PresenterTargetAvailability::Ready
                            {
                                frame_state.request_composition();
                            }
                        }
                        DrmRuntimeEvent::Command(command) => {
                            event_counts[5] += 1;
                            // Signal commands currently only request exit. Keep them on the
                            // shared path so future host commands cannot bypass backend policy.
                            exit_requested |= apply_host_command(
                                DrmHostCommandContext {
                                    children: &mut children,
                                    server: &mut loop_data.server,
                                    input_adapter: &mut loop_data.backend_state,
                                    events: &mut loop_data.events,
                                    shell: shell.as_mut(),
                                    output_metrics: &mut output_metrics,
                                    output_configurations: &mut output_configurations,
                                    output_layout_revision: &mut output_layout_revision,
                                    cursor: &mut cursor,
                                    frame_state: &mut frame_state,
                                    output_id: primary_output_id,
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
                cursor.refresh(&mut loop_data.server, &mut frame_state, Instant::now());
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

            let connected_outputs = output_monitor.physical_outputs(session_active);
            let presentable_outputs = presenters.presentable_outputs(&connected_outputs);
            let physical_output_available = !presentable_outputs.is_empty();
            let capture_ready = pending_capture
                .as_ref()
                .is_some_and(|capture| !capture.wait_for_client || shell.has_surface_frame());
            let composition_blocked = physical_output_available
                && !capture_ready
                && presenters.target_availability(&presentable_outputs)
                    == PresenterTargetAvailability::Busy;
            let work = iteration_work(frame_state.composition_due(now) && !composition_blocked);
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
                        exit_requested |= apply_host_command(
                            DrmHostCommandContext {
                                children: &mut children,
                                server: &mut loop_data.server,
                                input_adapter: &mut loop_data.backend_state,
                                events: &mut loop_data.events,
                                shell: shell.as_mut(),
                                output_metrics: &mut output_metrics,
                                output_configurations: &mut output_configurations,
                                output_layout_revision: &mut output_layout_revision,
                                cursor: &mut cursor,
                                frame_state: &mut frame_state,
                                output_id: primary_output_id,
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
                    presenters.suspend();
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
                                        presenters.activate_after_session(
                                            &output_monitor.physical_outputs(session_active),
                                        );
                                        frame_state.request_composition();
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

            cursor.refresh(&mut loop_data.server, &mut frame_state, now);

            if work.advance_main {
                loop_data.server.flush_pending_resizes();
                let connected_outputs = output_monitor.physical_outputs(session_active);
                let presentable_outputs = presenters.presentable_outputs(&connected_outputs);
                let physical_output_available = !presentable_outputs.is_empty();
                let capture_ready = pending_capture
                    .as_ref()
                    .is_some_and(|capture| !capture.wait_for_client || shell.has_surface_frame());
                let acquired_frames = (physical_output_available && !capture_ready)
                    .then(|| presenters.acquire_frames(&presentable_outputs))
                    .flatten();
                let requests = output_configurations
                    .iter()
                    .map(|output| {
                        let destination = acquired_frames
                            .as_ref()
                            .and_then(|frames| {
                                frames.iter().find(|(id, _)| *id == output.id()).map(
                                    |(_, frame)| {
                                        CompositionDestination::External(frame.target().clone())
                                    },
                                )
                            })
                            .unwrap_or(CompositionDestination::Owned);
                        CompositionOutputRequest {
                            output: output.id(),
                            destination,
                        }
                    })
                    .collect();
                let compositions = match shell.render_outputs(requests) {
                    Ok(compositions) => compositions,
                    Err(error) => {
                        if let Some(frames) = acquired_frames {
                            presenters.abort_frames(frames);
                        }
                        return Err(error).context("Bevy composition failed");
                    }
                };
                let callback_batch = loop_data.server.stage_frame_callbacks();
                loop_data.server.complete_frame_callbacks(callback_batch);
                frame_state.composition_rendered(now);

                if capture_ready && let Some(capture) = pending_capture.take() {
                    let _capture_span = tracing::trace_span!(
                        target: crate::PROFILE_TARGET,
                        "capture_readback_encode"
                    )
                    .entered();
                    let composition = compositions
                        .iter()
                        .find(|composition| composition.output == primary_output_id)
                        .map(|composition| &composition.frame)
                        .context("composition omitted the primary output")?;
                    let result = composition
                        .owned_texture()
                        .context("capture composition did not retain owned storage")
                        .and_then(|texture| {
                            read_composition_rgba(
                                &capture_device,
                                &capture_queue,
                                texture,
                                composition.target().extent().width,
                                composition.target().extent().height,
                                composition.target().format(),
                            )
                        })
                        .and_then(|pixels| {
                            write_png(
                                &capture.path,
                                composition.target().extent().width,
                                composition.target().extent().height,
                                &pixels,
                            )
                        })
                        .map_err(|error| error.to_string());
                    exit_requested |= complete_capture(shell.as_mut(), capture, result)?;
                }

                if let Some(frames) = acquired_frames {
                    match presenters.finish_frames(frames, &cursor.overlays) {
                        Ok(()) => frame_state.presented(),
                        Err(error) => {
                            error!(%error, "failed to finalize a physical output batch");
                            frame_state.request_composition();
                        }
                    }
                } else if physical_output_available
                    && presenters.target_availability(&presentable_outputs)
                        != PresenterTargetAvailability::Unavailable
                {
                    // Captures and transient presenter outages render into the
                    // retained target. The next frame must refresh scanout.
                    frame_state.request_composition();
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

        shutdown_presenter(
            &mut calloop,
            &mut loop_data,
            &mut presenters,
            &mut presenter_worker,
        )?;
        Ok(())
    }))
}

fn logical_coordinate(value: f64) -> Result<i32> {
    if !value.is_finite() || value < f64::from(i32::MIN) || value > f64::from(i32::MAX) {
        anyhow::bail!("output layout coordinate exceeds the Wayland integer range");
    }
    Ok(value.round() as i32)
}

fn logical_position(position: crate::surface::LogicalPoint) -> Result<(i32, i32)> {
    Ok((
        logical_coordinate(f64::from(position.x))?,
        logical_coordinate(f64::from(position.y))?,
    ))
}

/// Temporary standalone policy: non-primary outputs form a centered vertical
/// stack above the primary output. Topology coordinates retain fractional
/// shared edges; the Wayland protocol location is rounded separately.
fn center_primary_below_others(
    mut configurations: Vec<OutputConfiguration>,
) -> Result<Vec<OutputConfiguration>> {
    let widest_logical = configurations
        .iter()
        .map(|configuration| configuration.logical_width())
        .max_by(f64::total_cmp)
        .context("cannot place an empty DRM output layout")?;
    let widest_physical = configurations
        .iter()
        .map(|configuration| configuration.footprint().width_millimeters())
        .max_by(f64::total_cmp)
        .context("cannot place an empty DRM output footprint")?;
    let mut next_logical_y = 0.0;
    let mut next_physical_y = 0.0;

    for primary in [false, true] {
        for configuration in configurations
            .iter_mut()
            .filter(|configuration| configuration.is_primary() == primary)
        {
            let x = (widest_logical - configuration.logical_width()) * 0.5;
            let position = crate::surface::LogicalPoint::new(
                topology_coordinate(x)?,
                topology_coordinate(next_logical_y)?,
            );
            let footprint = configuration.footprint();
            let footprint_x = (widest_physical - footprint.width_millimeters()) * 0.5;
            *configuration = configuration
                .with_position(position)?
                .with_footprint_position(footprint_x, next_physical_y)?;
            next_logical_y = f64::from(position.y) + configuration.logical_height();
            next_physical_y += footprint.height_millimeters();
        }
    }

    Ok(configurations)
}

/// Converts topology coordinates without moving a stacked edge below its
/// exact predecessor. Below 65,536 logical pixels one f32 ULP is at most the
/// topology's 1/256-pixel portal epsilon, leaving ample room for real layouts.
fn topology_coordinate(value: f64) -> Result<f32> {
    logical_coordinate(value)?;
    let coordinate = value as f32;
    Ok(if f64::from(coordinate) < value {
        coordinate.next_up()
    } else {
        coordinate
    })
}

fn shutdown_presenter(
    calloop: &mut CalloopEventLoop<'static, LoopData<DrmRuntimeEvent, LibinputAdapter>>,
    loop_data: &mut LoopData<DrmRuntimeEvent, LibinputAdapter>,
    presenters: &mut Presenters,
    worker: &mut PresenterWorker,
) -> Result<()> {
    presenters.stop();
    worker.begin_shutdown();
    let deadline = Instant::now() + PRESENTER_SHUTDOWN_DEADLINE;
    let mut stopped = false;
    while !worker.finished() && Instant::now() < deadline {
        calloop
            .dispatch(
                Some(deadline.saturating_duration_since(Instant::now())),
                loop_data,
            )
            .context("DRM calloop dispatch failed during presenter shutdown")?;
        while let Some(event) = loop_data.events.pop_front() {
            if let DrmRuntimeEvent::Presenter(event) = event {
                stopped |= matches!(&event, PresenterEvent::Stopped);
                log_presenter_event(&event);
                presenters.handle_event(&event);
            }
        }
    }
    worker.join_if_finished();
    if !worker.finished() && !stopped {
        warn!(
            timeout_milliseconds = PRESENTER_SHUTDOWN_DEADLINE.as_millis(),
            "GBM/KMS presenter did not stop promptly; detaching until process teardown"
        );
    }
    Ok(())
}

struct DispatchTimeoutContext<'a> {
    frame_state: &'a FrameState,
    capture: Option<&'a PendingCapture>,
    remote_debug_enabled: bool,
    physical_output_available: bool,
    composition_blocked: bool,
    cursor_animation: Option<Instant>,
    now: Instant,
}

fn dispatch_timeout(context: DispatchTimeoutContext<'_>) -> Option<std::time::Duration> {
    let DispatchTimeoutContext {
        frame_state,
        capture,
        remote_debug_enabled,
        physical_output_available,
        composition_blocked,
        cursor_animation,
        now,
    } = context;
    let composition = if composition_blocked {
        frame_state
            .composition_demand_timeout(now)
            .map(|_| FRAME_INTERVAL)
    } else {
        frame_state.composition_demand_timeout(now)
    };
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
        PresenterEvent::WorkerReady => info!("GBM/KMS presenter worker is ready"),
        PresenterEvent::Frame(frame) => {
            trace!(?frame, "GBM/KMS presenter frame event")
        }
        PresenterEvent::OutputUnavailable { output, message } => {
            error!(?output, %message, "physical DRM presentation is unavailable")
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
        DispatchTimeoutContext, FrameState, MonitoredOutput, OutputConfiguration, OutputId,
        OutputLayout, OutputScale, OutputTopology, PresenterTargetAvailability,
        REMOTE_DEBUG_MAINTENANCE_INTERVAL, batch_target_availability, center_primary_below_others,
        dispatch_timeout, physical_output_ids, select_presentable_outputs,
    };
    use crate::{
        input::{InputDelta, InputPosition},
        output::OutputPhysicalSize,
        surface::{Extent, LogicalPoint},
    };
    use std::{
        collections::BTreeSet,
        time::{Duration, Instant},
    };

    fn output(id: u64, width: u32, height: u32, scale: f64, primary: bool) -> OutputConfiguration {
        OutputConfiguration::new(
            OutputId::new(id),
            Extent::new(width, height),
            OutputScale::new(scale).expect("valid scale"),
            LogicalPoint::ZERO,
            primary,
            None,
        )
        .expect("valid output")
    }

    fn measured_output(
        id: u64,
        width: u32,
        height: u32,
        width_millimeters: u32,
        height_millimeters: u32,
        scale: f64,
        primary: bool,
    ) -> OutputConfiguration {
        OutputConfiguration::new(
            OutputId::new(id),
            Extent::new(width, height),
            OutputScale::new(scale).expect("valid scale"),
            LogicalPoint::ZERO,
            primary,
            OutputPhysicalSize::new(width_millimeters, height_millimeters),
        )
        .expect("valid measured output")
    }

    #[test]
    fn mixed_dpi_layout_centers_the_primary_below_the_external_output() {
        let external = measured_output(2, 1_920, 1_080, 600, 340, 1.0, false);
        let primary = measured_output(1, 2_240, 1_400, 300, 190, 1.25, true);
        let placed = center_primary_below_others(vec![primary, external])
            .expect("mixed-DPI outputs should have a valid placement");
        let primary = placed
            .iter()
            .find(|output| output.is_primary())
            .expect("layout should retain its primary");
        let external = placed
            .iter()
            .find(|output| !output.is_primary())
            .expect("layout should retain its external output");

        assert_eq!(external.position(), LogicalPoint::ZERO);
        assert_eq!(primary.position(), LogicalPoint::new(64.0, 1_080.0));
        assert_eq!(external.footprint().x_millimeters(), 0.0);
        assert_eq!(primary.footprint().x_millimeters(), 150.0);

        let topology = OutputTopology::new(
            OutputLayout::new(1, placed).expect("centered rectangles should form a valid layout"),
        );
        let crossed_up = topology.move_pointer(
            InputPosition::new(65.0, 1_081.0),
            InputDelta::new(0.0, -10.0),
        );
        assert_eq!(topology.output_at(crossed_up), Some(OutputId::new(2)));
        let mapped_quarter = topology.move_pointer(
            InputPosition::new(512.0, 1_081.0),
            InputDelta::new(0.0, -10.0),
        );
        assert!((mapped_quarter.x - 720.0).abs() < 0.01);
        let blocked_overhang = topology.move_pointer(
            InputPosition::new(20.0, 1_079.0),
            InputDelta::new(0.0, 10.0),
        );
        assert_eq!(topology.output_at(blocked_overhang), Some(OutputId::new(2)));
    }

    #[test]
    fn changing_primary_scale_recalculates_its_centered_origin() {
        let outputs = center_primary_below_others(vec![
            measured_output(1, 2_240, 1_400, 300, 190, 1.25, true),
            measured_output(2, 1_920, 1_080, 600, 340, 1.0, false),
        ])
        .expect("initial placement should succeed");
        let outputs = outputs
            .into_iter()
            .map(|output| {
                if output.is_primary() {
                    output
                        .with_scale(OutputScale::new(1.5).expect("valid scale"))
                        .expect("scaled output should remain valid")
                } else {
                    output
                }
            })
            .collect();
        let outputs = center_primary_below_others(outputs).expect("reflow should succeed");
        let primary = outputs
            .iter()
            .find(|output| output.is_primary())
            .expect("layout should retain its primary");

        assert!((primary.position().x - 213.333_34).abs() < 0.000_1);
        assert_eq!(primary.position().y, 1_080.0);
        assert_eq!(primary.footprint().x_millimeters(), 150.0);
        assert_eq!(primary.footprint().y_millimeters(), 340.0);
        let primary_x = f64::from(primary.position().x);

        let topology = OutputTopology::new(
            OutputLayout::new(2, outputs).expect("scaled physical layout should remain valid"),
        );
        let crossed = topology.move_pointer(
            InputPosition::new(primary_x + 1.0, 1_081.0),
            InputDelta::new(0.0, -10.0),
        );
        assert_eq!(topology.output_at(crossed), Some(OutputId::new(2)));
    }

    #[test]
    fn physical_scale_match_equalizes_diagonal_logical_density() {
        let outputs = vec![
            measured_output(1, 2_240, 1_400, 300, 190, 1.0, true),
            measured_output(2, 1_920, 1_080, 600, 340, 1.0, false),
        ];

        let scale = super::matched_primary_scale(&outputs, OutputId::new(1))
            .expect("measured outputs should produce a scale match");
        let primary_density = super::measured_diagonal_dpi(outputs[0])
            .expect("primary DPI should be measured")
            / scale.value();
        let reference_density = super::measured_diagonal_dpi(outputs[1])
            .expect("reference DPI should be measured")
            / outputs[1].scale().value();

        assert!((primary_density - reference_density).abs() < 0.000_1);
    }

    #[test]
    fn physical_scale_match_rejects_assumed_dimensions() {
        let outputs = vec![
            output(1, 2_240, 1_400, 1.0, true),
            measured_output(2, 1_920, 1_080, 600, 340, 1.0, false),
        ];

        assert!(super::matched_primary_scale(&outputs, OutputId::new(1)).is_err());
    }

    #[test]
    fn physical_scale_match_rejects_implausible_measured_ratio() {
        let outputs = vec![
            measured_output(1, 2_240, 1_400, 3_000, 1_900, 1.0, true),
            measured_output(2, 1_920, 1_080, 600, 340, 1.0, false),
        ];

        assert!(super::matched_primary_scale(&outputs, OutputId::new(1)).is_err());
    }

    #[test]
    fn fractional_logical_heights_keep_stacked_pointer_portals_connected() {
        let placed = center_primary_below_others(vec![
            output(1, 2_240, 1_400, 1.25, true),
            output(2, 1_366, 768, 1.25, false),
            output(3, 1_920, 1_080, 1.0, false),
        ])
        .expect("fractional logical heights should remain placeable");
        let first = placed
            .iter()
            .find(|output| output.id() == OutputId::new(2))
            .expect("first external output should remain present");
        let second = placed
            .iter()
            .find(|output| output.id() == OutputId::new(3))
            .expect("second external output should remain present");
        let first_end = f64::from(first.position().y) + first.logical_height();
        let gap = f64::from(second.position().y) - first_end;
        assert!(
            gap >= 0.0,
            "stored origin must not overlap the prior output"
        );
        assert!(
            gap <= 1.0 / 256.0,
            "stored edge must remain a pointer portal"
        );

        let topology = OutputTopology::new(
            OutputLayout::new(1, placed)
                .expect("fractional shared edges should form a valid layout"),
        );
        let crossed = topology.move_pointer(
            InputPosition::new(960.0, first_end - 0.01),
            InputDelta::new(0.0, 1.0),
        );
        assert_eq!(topology.output_at(crossed), Some(OutputId::new(3)));
    }

    #[test]
    fn one_disconnected_output_leaves_the_other_physically_available() {
        let outputs = [
            MonitoredOutput {
                id: OutputId::new(1),
                connected: true,
                mode_compatible: true,
            },
            MonitoredOutput {
                id: OutputId::new(2),
                connected: false,
                mode_compatible: true,
            },
        ];

        assert_eq!(
            physical_output_ids(true, true, outputs.iter()),
            BTreeSet::from([OutputId::new(1)])
        );
    }

    #[test]
    fn unavailable_presenters_are_excluded_but_busy_presenters_block_the_batch() {
        let connected = BTreeSet::from([OutputId::new(1), OutputId::new(2)]);
        let presentable = select_presentable_outputs(&connected, |output| match output {
            output if output == OutputId::new(1) => Some(PresenterTargetAvailability::Ready),
            output if output == OutputId::new(2) => Some(PresenterTargetAvailability::Unavailable),
            _ => None,
        });

        assert_eq!(presentable, BTreeSet::from([OutputId::new(1)]));
        assert_eq!(
            batch_target_availability([
                PresenterTargetAvailability::Ready,
                PresenterTargetAvailability::Busy,
            ]),
            PresenterTargetAvailability::Busy
        );
    }

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
            composition_blocked: false,
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
            composition_blocked: false,
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
            composition_blocked: false,
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
                composition_blocked: false,
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
                composition_blocked: false,
                cursor_animation: Some(deadline),
                now,
            }),
            None
        );
    }

    #[test]
    fn busy_presenter_retries_pending_composition_at_a_bounded_rate() {
        let now = Instant::now();
        let frame_state = FrameState::default();

        assert_eq!(
            dispatch_timeout(DispatchTimeoutContext {
                frame_state: &frame_state,
                capture: None,
                remote_debug_enabled: false,
                physical_output_available: true,
                composition_blocked: true,
                cursor_animation: None,
                now,
            }),
            Some(crate::runtime::FRAME_INTERVAL)
        );
    }
}
