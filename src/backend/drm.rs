//! Standalone libseat/udev/libinput/DRM backend.
//!
//! Bevy remains the compositor renderer. This transitional backend reads its
//! completed texture into one memory element, then uses Pixman and Smithay's
//! [`DrmCompositor`] only to populate a GBM scanout buffer and submit KMS.

use std::{path::PathBuf, time::Instant};

use anyhow::{Context, Result, anyhow, bail};
use bevy::math::UVec2;
use calloop::signals::Signals;
use smithay::{
    backend::{
        SwapBuffersError,
        allocator::{
            Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmDeviceNotifier, DrmEvent,
            compositor::{DrmCompositor, FrameError, FrameFlags, RenderFrameError},
            exporter::gbm::{GbmFramebufferExporter, NodeFilter},
        },
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            ImportDma,
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            },
            pixman::PixmanRenderer,
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, primary_gpu},
    },
    output::{Mode as SmithayOutputMode, PhysicalProperties},
    reexports::{
        calloop::EventLoop as CalloopEventLoop,
        drm::control::{Mode as DrmMode, ModeTypeFlags, connector, crtc},
        gbm::Device as RawGbmDevice,
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::Display,
    },
    utils::{DeviceFd, Transform},
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{debug, error, info, warn};

use crate::{
    AppArguments,
    input::{
        raw::{RawSeatEvent, RawSeatEventKind},
        source::libinput::LibinputAdapter,
    },
    renderer::{WgpuContext, read_composition_rgba, write_png},
    runtime::{
        ChildProcesses, FrameState, HostCommand, LoopData, PendingCapture, iteration_work,
        server_mut,
    },
    server::{OutputDescriptor, OutputMetrics, ServerState},
    shell::{ShellRenderer, ShellRendererOptions},
};

type WeldDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    PresentedFrame,
    DrmDeviceFd,
>;

#[derive(Clone, Copy, Debug)]
struct PresentedFrame {
    sequence: u64,
    presentation_id: u64,
}

enum PresentOutcome {
    Queued(u64),
    Empty,
    Retry,
}

enum DrmRuntimeEvent {
    Input(RawSeatEvent),
    VBlank(crtc::Handle),
    DrmError(String),
    Session(SessionEvent),
    Udev(UdevEvent),
    Command(HostCommand),
}

struct DrmBootstrap {
    session: LibSeatSession,
    notifier: Option<DrmDeviceNotifier>,
    drm: DrmDevice,
    gbm: RawGbmDevice<DrmDeviceFd>,
    scanner: DrmScanner,
    device_id: smithay::reexports::rustix::fs::Dev,
    device_path: PathBuf,
    connector: connector::Info,
    crtc: crtc::Handle,
    mode: DrmMode,
}

struct DrmPresenter {
    _session: LibSeatSession,
    drm: DrmDevice,
    compositor: WeldDrmCompositor,
    pixman: PixmanRenderer,
    frame: MemoryRenderBuffer,
    crtc: crtc::Handle,
    connector_name: String,
    device_id: smithay::reexports::rustix::fs::Dev,
    device_path: PathBuf,
    scanner: DrmScanner,
    session_active: bool,
    frame_pending: bool,
    next_sequence: u64,
    pending_capture: Option<(u64, PendingCapture)>,
    pixels: Vec<u8>,
    presentation_id: u64,
}

pub(crate) fn run(arguments: AppArguments, signals: Signals) -> Result<()> {
    let started_at = Instant::now();
    let mut calloop: CalloopEventLoop<'static, LoopData<DrmRuntimeEvent>> =
        CalloopEventLoop::try_new().context("failed to create the DRM calloop event loop")?;

    let (session, session_notifier) =
        LibSeatSession::new().context("failed to acquire a libseat session")?;
    let seat_name = session.seat();
    let udev = UdevBackend::new(&seat_name).context("failed to initialize DRM udev discovery")?;
    let mut bootstrap = DrmBootstrap::new(session, udev.device_list())?;
    let (output_descriptor, output_metrics) = bootstrap.output_description()?;

    let display = Display::<ServerState>::new().context("failed to create the Wayland display")?;
    let server = ServerState::new(
        &calloop.handle(),
        display,
        started_at,
        &seat_name,
        output_descriptor,
        output_metrics,
        server_mut::<DrmRuntimeEvent>,
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
        bootstrap.session.clone().into(),
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
    let drm_notifier = bootstrap
        .notifier
        .take()
        .context("DRM notifier was already installed")?;
    let drm_registration = calloop
        .handle()
        .insert_source(drm_notifier, |event, _, data| match event {
            DrmEvent::VBlank(crtc) => data.events.push_back(DrmRuntimeEvent::VBlank(crtc)),
            DrmEvent::Error(error) => data
                .events
                .push_back(DrmRuntimeEvent::DrmError(error.to_string())),
        })
        .context("failed to register DRM events")?;

    let result = run_active_drm(
        &mut calloop,
        &mut loop_data,
        bootstrap,
        started_at,
        arguments,
        output_metrics,
    );
    // The notifier owns the last device Arc after the presenter drops. Remove it
    // while calloop's libseat notifier still owns the strong seat reference, so
    // atomic state restoration retains DRM authority.
    calloop.handle().remove(drm_registration);
    result
}

fn run_active_drm(
    calloop: &mut CalloopEventLoop<'static, LoopData<DrmRuntimeEvent>>,
    loop_data: &mut LoopData<DrmRuntimeEvent>,
    bootstrap: DrmBootstrap,
    started_at: Instant,
    arguments: AppArguments,
    output_metrics: OutputMetrics,
) -> Result<()> {
    let mut presenter = bootstrap.into_presenter(loop_data.server.output())?;
    let gpu = WgpuContext::headless()?;
    let mut shell = ShellRenderer::new(
        gpu.instance(),
        gpu.adapter(),
        gpu.device(),
        gpu.queue(),
        ShellRendererOptions {
            size: UVec2::new(
                output_metrics.physical_width(),
                output_metrics.physical_height(),
            ),
            scale_factor: output_metrics.scale_factor(),
            remote_debug: arguments.remote_debug.as_deref(),
            software_cursor: true,
        },
    )?;
    let mut children = ChildProcesses::default();
    let child_requested = children.spawn_requested(&loop_data.server, &arguments.client)?;
    let mut pending_capture = arguments
        .screenshot
        .map(|path| PendingCapture::startup(path, child_requested));
    let remote_debug_enabled = arguments.remote_debug.is_some();
    let mut frame_state = FrameState::default();

    info!(
        socket = ?loop_data.server.socket_name,
        connector = %presenter.connector_name(),
        "Weld DRM compositor is ready"
    );
    let mut exit_requested = false;
    while !exit_requested {
        calloop
            .dispatch(
                Some(frame_state.composition_timeout(Instant::now(), presenter.session_active)),
                loop_data,
            )
            .context("DRM calloop dispatch failed")?;

        let mut input_pending = false;
        while let Some(event) = loop_data.events.pop_front() {
            match event {
                DrmRuntimeEvent::Input(event) => {
                    input_pending = true;
                    shell.enqueue_input_event(event);
                }
                DrmRuntimeEvent::VBlank(crtc) => {
                    if let Some(frame) = presenter.frame_submitted(crtc)? {
                        loop_data.server.frame_presented(frame.presentation_id);
                        if let Some((capture_sequence, capture)) = presenter.pending_capture.take()
                        {
                            if capture_sequence == frame.sequence {
                                exit_requested |= complete_capture(&mut shell, capture, Ok(()))?;
                            } else {
                                presenter.pending_capture = Some((capture_sequence, capture));
                            }
                        }
                    }
                }
                DrmRuntimeEvent::DrmError(message) => warn!(%message, "DRM event error"),
                DrmRuntimeEvent::Session(SessionEvent::PauseSession) => {
                    presenter.pause();
                    input_pending = true;
                    shell.enqueue_input_event(RawSeatEvent::new(
                        RawSeatEventKind::HostFocusLost,
                        started_at.elapsed().as_millis() as u32,
                    ));
                }
                DrmRuntimeEvent::Session(SessionEvent::ActivateSession) => {
                    presenter.activate()?;
                    frame_state.request_composition();
                    frame_state.request_present();
                }
                DrmRuntimeEvent::Udev(event) => presenter.handle_udev(event)?,
                DrmRuntimeEvent::Command(command) => {
                    exit_requested |= children.apply(&loop_data.server, command)?;
                }
            }
        }
        if exit_requested {
            break;
        }

        for event in loop_data.server.take_surface_events() {
            shell.enqueue_surface_event(event);
            frame_state.request_composition();
        }
        if loop_data.server.presentation_requested() {
            frame_state.request_present();
        }

        let now = Instant::now();
        let work = iteration_work(
            input_pending,
            frame_state.composition_due(now),
            remote_debug_enabled,
            presenter.session_active,
        );
        let mut request_next_composition = false;
        if work.advance_main {
            let bevy_requested_redraw = shell.advance_main(
                started_at.elapsed().as_millis() as u32,
                work.composition_advance,
            );
            for action in shell.take_surface_actions() {
                loop_data.server.apply_surface_action(action);
            }
            for effect in shell.take_input_effects() {
                loop_data.server.apply_input_effect(effect);
            }
            for command in shell.take_host_commands() {
                exit_requested |= children.apply(&loop_data.server, command)?;
            }
            if bevy_requested_redraw && !work.composition_advance {
                frame_state.request_composition();
            }
            if shell.should_exit() || exit_requested {
                break;
            }
            if work.composition_advance {
                shell.render_composition();
                let pixels = read_composition_rgba(
                    gpu.device(),
                    gpu.queue(),
                    shell.texture(),
                    output_metrics.physical_width(),
                    output_metrics.physical_height(),
                )?;
                let presentation_id = loop_data.server.stage_surface_presentation();
                // A startup capture may wait across several client compositions;
                // retaining pixels only while that pre-submit slot exists keeps
                // ordinary cursor and animation frames out of the extra memcpy.
                presenter.update_frame(&pixels, presentation_id, pending_capture.is_some())?;
                frame_state.composition_rendered(now);
                request_next_composition = bevy_requested_redraw;
            }
        }

        if pending_capture.is_none()
            && presenter.pending_capture.is_none()
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
                &mut shell,
                capture,
                Err("screenshot timed out before DRM presentation".to_owned()),
            )?;
        }
        if presenter
            .pending_capture
            .as_ref()
            .is_some_and(|(_, capture)| capture.deadline <= Instant::now())
            && let Some((_, capture)) = presenter.pending_capture.take()
        {
            exit_requested |= complete_capture(
                &mut shell,
                capture,
                Err("screenshot timed out waiting for a DRM page flip".to_owned()),
            )?;
        }

        if presenter.session_active && frame_state.presentation_due() && !presenter.frame_pending {
            let capture_ready = pending_capture
                .as_ref()
                .is_some_and(|capture| !capture.wait_for_client || shell.has_surface_frame());
            match presenter.queue_frame()? {
                PresentOutcome::Queued(sequence) => {
                    frame_state.presented();
                    if capture_ready && let Some(capture) = pending_capture.take() {
                        let capture_result = write_png(
                            &capture.path,
                            output_metrics.physical_width(),
                            output_metrics.physical_height(),
                            presenter.frame_pixels(),
                        )
                        .map_err(|error| error.to_string());
                        presenter.clear_capture_pixels();
                        if capture_result.is_ok() {
                            presenter.pending_capture = Some((sequence, capture));
                        } else {
                            exit_requested |=
                                complete_capture(&mut shell, capture, capture_result)?;
                        }
                    }
                }
                PresentOutcome::Empty => frame_state.presented(),
                PresentOutcome::Retry => {}
            }
        }
        if request_next_composition {
            frame_state.request_composition();
        }
        loop_data.server.flush_clients();
        children.reap();
    }

    Ok(())
}

impl DrmBootstrap {
    fn new<'a>(
        mut session: LibSeatSession,
        devices: impl Iterator<Item = (smithay::reexports::rustix::fs::Dev, &'a std::path::Path)>,
    ) -> Result<Self> {
        let primary =
            primary_gpu(session.seat())?.context("no DRM GPU was found for the active seat")?;
        let devices = devices
            .map(|(device_id, path)| (device_id, path.to_path_buf()))
            .collect::<Vec<_>>();
        let (device_id, device_path) = devices
            .iter()
            .find(|(_, path)| *path == primary)
            .cloned()
            .or_else(|| devices.first().cloned())
            .context("udev reported no DRM devices for the active seat")?;
        let fd = session
            .open(
                &device_path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .with_context(|| format!("failed to open DRM device {}", device_path.display()))?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let (drm, notifier) =
            DrmDevice::new(fd.clone(), true).context("failed to initialize the DRM device")?;
        let gbm = GbmDevice::new(fd).context("failed to initialize GBM")?;
        let mut scanner = DrmScanner::new();
        let (connector, crtc) = scanner
            .scan_connectors(&drm)?
            .into_iter()
            .find_map(|event| match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } if !connector.modes().is_empty() => Some((connector, crtc)),
                _ => None,
            })
            .context("no connected DRM connector with a usable CRTC and mode")?;
        let mode = preferred_mode(&connector)?;
        Ok(Self {
            session,
            notifier: Some(notifier),
            drm,
            gbm,
            scanner,
            device_id,
            device_path,
            connector,
            crtc,
            mode,
        })
    }

    fn output_description(&self) -> Result<(OutputDescriptor, OutputMetrics)> {
        let name = connector_name(&self.connector);
        let (physical_width, physical_height) = self.connector.size().unwrap_or((0, 0));
        let wl_mode = SmithayOutputMode::from(self.mode);
        let metrics = OutputMetrics::new(
            u32::try_from(wl_mode.size.w).context("negative DRM mode width")?,
            u32::try_from(wl_mode.size.h).context("negative DRM mode height")?,
            1.0,
        )?
        .with_refresh_millihertz(wl_mode.refresh)?;
        let descriptor = OutputDescriptor {
            name,
            physical_properties: PhysicalProperties {
                size: (
                    i32::try_from(physical_width).context("physical output width exceeds i32")?,
                    i32::try_from(physical_height).context("physical output height exceeds i32")?,
                )
                    .into(),
                subpixel: self.connector.subpixel().into(),
                make: "Unknown".to_owned(),
                model: "Unknown".to_owned(),
                serial_number: "Unknown".to_owned(),
            },
        };
        Ok((descriptor, metrics))
    }

    fn into_presenter(mut self, output: smithay::output::Output) -> Result<DrmPresenter> {
        let surface = self
            .drm
            .create_surface(self.crtc, self.mode, &[self.connector.handle()])
            .context("failed to create the DRM output surface")?;
        let pixman = PixmanRenderer::new().context("failed to initialize Pixman")?;
        let allocator = GbmAllocator::new(
            self.gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let exporter = GbmFramebufferExporter::new(self.gbm.clone(), NodeFilter::None);
        let compositor = DrmCompositor::new(
            &output,
            surface,
            None,
            allocator,
            exporter,
            [Fourcc::Abgr8888, Fourcc::Xbgr8888],
            pixman.dmabuf_formats(),
            self.drm.cursor_size(),
            None,
        )
        .map_err(|error| anyhow!("failed to initialize DRM compositor: {error}"))?;
        let size = self.mode.size();
        let frame = MemoryRenderBuffer::new(
            Fourcc::Abgr8888,
            (i32::from(size.0), i32::from(size.1)),
            1,
            Transform::Normal,
            None,
        );
        Ok(DrmPresenter {
            _session: self.session,
            drm: self.drm,
            compositor,
            pixman,
            frame,
            crtc: self.crtc,
            connector_name: connector_name(&self.connector),
            device_id: self.device_id,
            device_path: self.device_path,
            scanner: self.scanner,
            session_active: true,
            frame_pending: false,
            next_sequence: 1,
            pending_capture: None,
            pixels: Vec::new(),
            presentation_id: 0,
        })
    }
}

impl DrmPresenter {
    fn connector_name(&self) -> String {
        self.connector_name.clone()
    }

    fn update_frame(
        &mut self,
        pixels: &[u8],
        presentation_id: u64,
        retain_capture_pixels: bool,
    ) -> Result<()> {
        let size = self.compositor.current_mode().size();
        let damage =
            smithay::utils::Rectangle::from_size((i32::from(size.0), i32::from(size.1)).into());
        self.frame.render().draw(|target| {
            if target.len() != pixels.len() {
                bail!(
                    "DRM memory frame has {} bytes but Bevy produced {}",
                    target.len(),
                    pixels.len()
                );
            }
            target.copy_from_slice(pixels);
            Ok(vec![damage])
        })?;
        if retain_capture_pixels {
            self.pixels.clear();
            self.pixels.extend_from_slice(pixels);
        }
        self.presentation_id = presentation_id;
        Ok(())
    }

    fn frame_pixels(&self) -> &[u8] {
        &self.pixels
    }

    fn clear_capture_pixels(&mut self) {
        self.pixels.clear();
    }

    fn queue_frame(&mut self) -> Result<PresentOutcome> {
        let element = MemoryRenderBufferRenderElement::from_buffer(
            &mut self.pixman,
            (0.0, 0.0),
            &self.frame,
            None,
            None,
            None,
            Kind::Unspecified,
        )?;
        match self.compositor.render_frame(
            &mut self.pixman,
            &[element],
            [0.0, 0.0, 0.0, 1.0],
            FrameFlags::empty(),
        ) {
            Ok(_) => {}
            Err(RenderFrameError::PrepareFrame(FrameError::EmptyFrame)) => {
                debug!("DRM composition produced no scanout changes");
                return Ok(PresentOutcome::Empty);
            }
            Err(RenderFrameError::PrepareFrame(FrameError::NoFreeSlotsError)) => {
                warn!("DRM swapchain has no free slot; deferring presentation");
                return Ok(PresentOutcome::Retry);
            }
            Err(RenderFrameError::PrepareFrame(error)) => {
                let description = error.to_string();
                match SwapBuffersError::from(error) {
                    SwapBuffersError::TemporaryFailure(_) => {
                        debug!(error = %description, "temporary DRM render failure");
                        return Ok(PresentOutcome::Retry);
                    }
                    SwapBuffersError::AlreadySwapped | SwapBuffersError::ContextLost(_) => {
                        bail!("failed to render DRM frame: {description}")
                    }
                }
            }
            Err(RenderFrameError::RenderFrame(error)) => {
                bail!("failed to render DRM frame: {error}")
            }
        }
        let sequence = self.next_sequence;
        match self.compositor.queue_frame(PresentedFrame {
            sequence,
            presentation_id: self.presentation_id,
        }) {
            Ok(()) => {}
            Err(FrameError::EmptyFrame) => {
                debug!("DRM compositor declined an empty frame");
                return Ok(PresentOutcome::Empty);
            }
            Err(FrameError::NoFreeSlotsError) => {
                warn!("DRM swapchain has no free slot; deferring page flip");
                return Ok(PresentOutcome::Retry);
            }
            Err(error) => {
                let description = error.to_string();
                match SwapBuffersError::from(error) {
                    SwapBuffersError::TemporaryFailure(_) => {
                        debug!(error = %description, "temporary DRM page-flip failure");
                        return Ok(PresentOutcome::Retry);
                    }
                    SwapBuffersError::AlreadySwapped | SwapBuffersError::ContextLost(_) => {
                        bail!("failed to queue DRM frame: {description}")
                    }
                }
            }
        }
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.frame_pending = true;
        Ok(PresentOutcome::Queued(sequence))
    }

    fn frame_submitted(&mut self, crtc: crtc::Handle) -> Result<Option<PresentedFrame>> {
        if crtc != self.crtc {
            return Ok(None);
        }
        let sequence = self
            .compositor
            .frame_submitted()
            .map_err(|error| anyhow!("failed to retire DRM frame: {error}"))?;
        self.frame_pending = false;
        Ok(sequence)
    }

    fn pause(&mut self) {
        self.session_active = false;
        self.drm.pause();
        self.frame_pending = false;
    }

    fn activate(&mut self) -> Result<()> {
        self.drm
            .activate(true)
            .context("failed to reactivate DRM after VT switch")?;
        self.compositor
            .clear()
            .context("failed to discard the pre-pause DRM frame")?;
        self.compositor
            .reset_state()
            .context("failed to reset DRM output after VT switch")?;
        self.session_active = true;
        Ok(())
    }

    fn handle_udev(&mut self, event: UdevEvent) -> Result<()> {
        match event {
            UdevEvent::Changed { device_id } if device_id == self.device_id => {
                for event in self.scanner.scan_connectors(&self.drm)? {
                    match event {
                        DrmScanEvent::Disconnected {
                            crtc: Some(crtc), ..
                        } if crtc == self.crtc => {
                            bail!("the active DRM connector was disconnected")
                        }
                        DrmScanEvent::Changed {
                            crtc: Some(crtc), ..
                        } if crtc == self.crtc => {
                            warn!("active connector modes changed; restart Weld to select a mode")
                        }
                        _ => {}
                    }
                }
            }
            UdevEvent::Removed { device_id } if device_id == self.device_id => {
                bail!(
                    "the selected DRM device {} was removed",
                    self.device_path.display()
                )
            }
            UdevEvent::Added { device_id, path } => {
                warn!(?device_id, path = %path.display(), "additional DRM GPUs are not supported yet")
            }
            UdevEvent::Changed { .. } | UdevEvent::Removed { .. } => {}
        }
        Ok(())
    }
}

fn preferred_mode(connector: &connector::Info) -> Result<DrmMode> {
    connector
        .modes()
        .iter()
        .copied()
        .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector.modes().first().copied())
        .context("DRM connector has no modes")
}

fn connector_name(connector: &connector::Info) -> String {
    format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    )
}

fn complete_capture(
    shell: &mut ShellRenderer,
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
