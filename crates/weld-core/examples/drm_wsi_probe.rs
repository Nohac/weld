//! Direct Vulkan DRM presentation probe.
//!
//! This intentionally bypasses Weld and Smithay's DRM compositor. It verifies
//! that wgpu can own the selected connector, present without a CPU copy, and
//! survive a libseat pause/activate cycle before that path is integrated into
//! the compositor.

use std::{
    collections::VecDeque,
    os::fd::AsRawFd,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use ash::{ext, khr, vk};
use calloop::{
    EventLoop,
    signals::{Signal, Signals},
};
use clap::Parser;
use smithay::{
    backend::{
        drm::DrmDeviceFd,
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, primary_gpu},
    },
    output::Mode as SmithayOutputMode,
    reexports::{
        drm::control::{Mode, ModeTypeFlags, connector},
        rustix::fs::{Dev, OFlags, major, minor},
    },
    utils::DeviceFd,
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{info, warn};

const MAX_CONSECUTIVE_SURFACE_FAILURES: u8 = 3;
const ACTIVE_EVENT_DISPATCH_INTERVAL: Duration = Duration::from_millis(2);
const SESSION_EVENT_DEADLINE: Duration = Duration::from_secs(2);
const SLOW_ACQUIRE_THRESHOLD: Duration = Duration::from_millis(100);

#[derive(Debug, Parser)]
#[command(about = "Validate zero-copy wgpu presentation on a DRM connector")]
struct Arguments {
    /// Maximum probe duration before restoring the console.
    #[arg(long, default_value_t = 60)]
    seconds: u64,

    /// Ask libseat to switch to this VT after five seconds.
    #[arg(long)]
    switch_vt: Option<i32>,
}

#[derive(Clone, Copy, Debug)]
struct DirectMode {
    plane_index: u32,
    connector_id: u32,
    width: u32,
    height: u32,
    refresh_millihertz: u32,
}

#[derive(Clone, Copy, Debug)]
enum ProbeEvent {
    Exit,
    Session(SessionEvent),
}

#[derive(Default)]
struct ProbeEvents(VecDeque<ProbeEvent>);

struct PendingSessionRecovery {
    lost_at: Instant,
    pause_observed: bool,
}

struct ProbeSummary {
    started_at: Instant,
    frames_presented: u64,
    frames_presented_after_resume: u64,
    pause_observed: bool,
    resume_observed: bool,
    direct_mode: DirectMode,
    format: wgpu::TextureFormat,
    present_mode: wgpu::PresentMode,
}

impl Drop for ProbeSummary {
    fn drop(&mut self) {
        let elapsed = self.started_at.elapsed();
        let average_fps = self.frames_presented as f64 / elapsed.as_secs_f64().max(f64::EPSILON);
        info!(
            frames_presented = self.frames_presented,
            frames_presented_after_resume = self.frames_presented_after_resume,
            elapsed_seconds = elapsed.as_secs_f64(),
            average_fps,
            pause_observed = self.pause_observed,
            resume_observed = self.resume_observed,
            plane_index = self.direct_mode.plane_index,
            width = self.direct_mode.width,
            height = self.direct_mode.height,
            refresh_millihertz = self.direct_mode.refresh_millihertz,
            format = ?self.format,
            present_mode = ?self.present_mode,
            "direct DRM probe finished"
        );
    }
}

enum PresentOutcome {
    Presented,
    PresentedSuboptimal,
    Deferred(&'static str),
    Reconfigure(&'static str),
    Lost,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM])
        .map_err(|error| anyhow!("failed to initialize signal handling: {error}"))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init()
        .map_err(|error| anyhow!("failed to initialize tracing: {error}"))?;

    let mut event_loop =
        EventLoop::<ProbeEvents>::try_new().context("failed to create the probe event loop")?;
    event_loop
        .handle()
        .insert_source(signals, |_, _, events| {
            events.0.push_back(ProbeEvent::Exit);
        })
        .context("failed to register signal handling")?;

    let (mut session, session_notifier) =
        LibSeatSession::new().context("failed to acquire a libseat session")?;
    let seat_name = session.seat();
    let udev = UdevBackend::new(&seat_name).context("failed to initialize udev discovery")?;
    let (device_id, device_path) = select_device(&mut session, udev.device_list())?;
    let fd = session
        .open(
            &device_path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .with_context(|| format!("failed to open DRM device {}", device_path.display()))?;
    let drm = DrmDeviceFd::new(DeviceFd::from(fd));

    let (connector, mode) = select_output(&drm)?;
    let connector_name = format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    );
    event_loop
        .handle()
        .insert_source(session_notifier, |event, _, events| {
            events.0.push_back(ProbeEvent::Session(event));
        })
        .map_err(|_| anyhow!("failed to register libseat notifications"))?;

    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
    descriptor.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(descriptor);
    let adapter = select_vulkan_adapter(&instance, device_id)?;
    let direct_mode = inspect_direct_mode(
        &adapter,
        drm.as_raw_fd(),
        u32::from(connector.handle()),
        mode,
    )?;
    info!(
        node = %device_path.display(),
        connector = %connector_name,
        plane_index = direct_mode.plane_index,
        width = direct_mode.width,
        height = direct_mode.height,
        vulkan_refresh_millihertz = direct_mode.refresh_millihertz,
        adapter = ?adapter.get_info(),
        "validated direct DRM display parameters"
    );

    // SAFETY: libseat keeps the DRM fd alive, the connector and mode were
    // discovered from that fd, and `inspect_direct_mode` selected a Vulkan
    // display plane that advertises support for this exact display.
    let surface = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::Drm {
            fd: drm.as_raw_fd(),
            plane: direct_mode.plane_index,
            connector_id: direct_mode.connector_id,
            width: direct_mode.width,
            height: direct_mode.height,
            refresh_rate: direct_mode.refresh_millihertz,
        })
    }))
    .map_err(|_| anyhow!("wgpu panicked while creating the direct DRM surface"))?
    .context("wgpu could not create the direct DRM surface")?;

    let compatible_adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .context("no Vulkan adapter can present to the direct DRM surface")?;
    ensure_same_adapter(&adapter, &compatible_adapter)?;
    let (device, queue) =
        pollster::block_on(compatible_adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("weld direct DRM probe device"),
            ..Default::default()
        }))
        .context("failed to create the direct DRM probe device")?;

    let capabilities = surface.get_capabilities(&compatible_adapter);
    let mut surface_config = surface
        .get_default_config(&compatible_adapter, direct_mode.width, direct_mode.height)
        .context("adapter does not support the direct DRM surface")?;
    surface_config.format = capabilities
        .formats
        .iter()
        .copied()
        .find(wgpu::TextureFormat::is_srgb)
        .context("direct DRM surface has no sRGB texture format")?;
    if !capabilities
        .present_modes
        .contains(&wgpu::PresentMode::Fifo)
    {
        bail!("direct DRM surface does not support FIFO presentation");
    }
    surface_config.present_mode = wgpu::PresentMode::Fifo;
    surface_config.desired_maximum_frame_latency = 1;
    info!(
        formats = ?capabilities.formats,
        present_modes = ?capabilities.present_modes,
        alpha_modes = ?capabilities.alpha_modes,
        selected_format = ?surface_config.format,
        selected_present_mode = ?surface_config.present_mode,
        "configuring direct DRM surface"
    );
    configure_direct_surface(
        &surface,
        &device,
        &surface_config,
        "initial direct DRM surface configuration failed",
    )?;

    let refresh_interval = Duration::from_nanos(
        1_000_000_000_000_u64 / u64::from(direct_mode.refresh_millihertz.max(1)),
    );
    let started_at = Instant::now();
    let deadline = started_at + Duration::from_secs(arguments.seconds);
    let switch_at = arguments
        .switch_vt
        .map(|_| started_at + Duration::from_secs(5));
    let mut events = ProbeEvents::default();
    let mut active = session.is_active();
    let mut switch_requested = false;
    let mut consecutive_surface_failures = 0_u8;
    let mut pending_session_recovery: Option<PendingSessionRecovery> = None;
    let mut first_acquisition_after_activation = false;
    let mut first_present_after_activation = false;
    let mut summary = ProbeSummary {
        started_at,
        frames_presented: 0,
        frames_presented_after_resume: 0,
        pause_observed: false,
        resume_observed: false,
        direct_mode,
        format: surface_config.format,
        present_mode: surface_config.present_mode,
    };
    if let Some(vt) = arguments.switch_vt {
        info!(
            vt,
            "direct DRM probe is presenting; automatic VT switch scheduled in five seconds"
        );
    } else {
        info!("direct DRM probe is presenting; it will exit at the configured deadline");
    }
    'running: loop {
        if Instant::now() >= deadline {
            break;
        }
        if pending_session_recovery.as_ref().is_some_and(|recovery| {
            !recovery.pause_observed && recovery.lost_at.elapsed() >= SESSION_EVENT_DEADLINE
        }) {
            bail!(
                "the DRM surface was lost, but libseat delivered no PauseSession event within {} ms",
                SESSION_EVENT_DEADLINE.as_millis()
            );
        }
        if active
            && !switch_requested
            && switch_at.is_some_and(|switch_at| Instant::now() >= switch_at)
            && let Some(vt) = arguments.switch_vt
        {
            info!(vt, "requesting probe VT switch through libseat");
            // Stop presentation before asking libseat to switch. Acquiring a
            // FIFO image while calloop needs to acknowledge session disable
            // can otherwise leave Mesa and seatd waiting on each other.
            active = false;
            session
                .change_vt(vt)
                .with_context(|| format!("failed to switch to VT {vt}"))?;
            switch_requested = true;
        }
        let awaiting_pause_after_loss = pending_session_recovery
            .as_ref()
            .is_some_and(|recovery| !recovery.pause_observed);
        let dispatch_timeout = if active || awaiting_pause_after_loss {
            ACTIVE_EVENT_DISPATCH_INTERVAL
        } else {
            refresh_interval
        };
        event_loop
            .dispatch(
                Some(dispatch_timeout.min(deadline.saturating_duration_since(Instant::now()))),
                &mut events,
            )
            .context("probe event dispatch failed")?;
        while let Some(event) = events.0.pop_front() {
            match event {
                ProbeEvent::Exit => break 'running,
                ProbeEvent::Session(SessionEvent::PauseSession) => {
                    active = false;
                    summary.pause_observed = true;
                    if let Some(recovery) = pending_session_recovery.as_mut() {
                        recovery.pause_observed = true;
                        info!(
                            elapsed_milliseconds = recovery.lost_at.elapsed().as_millis(),
                            "libseat pause arrived after the DRM surface reported lost"
                        );
                    } else {
                        info!("session paused by libseat; direct presentation stopped");
                    }
                }
                ProbeEvent::Session(SessionEvent::ActivateSession) => {
                    if pending_session_recovery
                        .as_ref()
                        .is_some_and(|recovery| !recovery.pause_observed)
                    {
                        bail!(
                            "libseat activated the session after surface loss without delivering PauseSession"
                        );
                    }
                    let recovering_from_lost = pending_session_recovery.take().is_some();
                    configure_direct_surface(
                        &surface,
                        &device,
                        &surface_config,
                        "reconfiguring the existing DRM surface after activation failed",
                    )?;
                    active = true;
                    summary.resume_observed = summary.pause_observed;
                    consecutive_surface_failures = 0;
                    first_acquisition_after_activation = true;
                    first_present_after_activation = true;
                    info!(
                        recovering_from_lost,
                        "session activated by libseat; existing direct surface reconfigured"
                    );
                }
            }
        }
        if active {
            let outcome = present_clear_frame(&surface, &device, &queue, summary.frames_presented)?;
            let was_first_acquisition_after_activation =
                std::mem::take(&mut first_acquisition_after_activation);
            match outcome {
                PresentOutcome::Presented => {
                    summary.frames_presented = summary.frames_presented.saturating_add(1);
                    if summary.resume_observed {
                        summary.frames_presented_after_resume =
                            summary.frames_presented_after_resume.saturating_add(1);
                    }
                    if first_present_after_activation {
                        info!("presented the first frame after session activation");
                        first_present_after_activation = false;
                    }
                    consecutive_surface_failures = 0;
                }
                PresentOutcome::PresentedSuboptimal => {
                    summary.frames_presented = summary.frames_presented.saturating_add(1);
                    if summary.resume_observed {
                        summary.frames_presented_after_resume =
                            summary.frames_presented_after_resume.saturating_add(1);
                    }
                    if first_present_after_activation {
                        info!("presented the first frame after session activation");
                        first_present_after_activation = false;
                    }
                    consecutive_surface_failures = 0;
                    configure_direct_surface(
                        &surface,
                        &device,
                        &surface_config,
                        "reconfiguring a suboptimal direct DRM surface failed",
                    )?;
                    info!("reconfigured suboptimal direct DRM surface after present");
                }
                PresentOutcome::Deferred(reason) => {
                    consecutive_surface_failures = consecutive_surface_failures.saturating_add(1);
                    if consecutive_surface_failures >= MAX_CONSECUTIVE_SURFACE_FAILURES {
                        bail!(
                            "direct DRM surface acquisition failed {consecutive_surface_failures} consecutive times ({reason})"
                        );
                    }
                    warn!(
                        reason,
                        consecutive_surface_failures,
                        "direct DRM surface acquisition deferred; yielding to the event loop"
                    );
                }
                PresentOutcome::Reconfigure(reason) => {
                    consecutive_surface_failures = consecutive_surface_failures.saturating_add(1);
                    if consecutive_surface_failures >= MAX_CONSECUTIVE_SURFACE_FAILURES {
                        bail!(
                            "direct DRM surface stayed outdated for {consecutive_surface_failures} consecutive acquisitions ({reason})"
                        );
                    }
                    warn!(
                        reason,
                        consecutive_surface_failures,
                        "reconfiguring outdated direct DRM surface before the next event-loop iteration"
                    );
                    configure_direct_surface(
                        &surface,
                        &device,
                        &surface_config,
                        "reconfiguring an outdated direct DRM surface failed",
                    )?;
                }
                PresentOutcome::Lost => {
                    if was_first_acquisition_after_activation {
                        bail!(
                            "the existing DRM surface remained lost after libseat activation and reconfiguration"
                        );
                    }
                    active = false;
                    pending_session_recovery = Some(PendingSessionRecovery {
                        lost_at: Instant::now(),
                        pause_observed: false,
                    });
                    warn!(
                        "the DRM surface was lost before calloop observed the session transition; yielding for libseat events"
                    );
                }
            }
        }
    }

    // Keep all wgpu objects in this scope so they are dropped before `drm`
    // releases the libseat-owned fd on return.
    drop(surface);
    drop(queue);
    drop(device);
    drop(compatible_adapter);
    drop(adapter);
    drop(instance);
    if arguments.switch_vt.is_some() && summary.frames_presented_after_resume == 0 {
        bail!("the requested VT cycle completed without a successful post-resume presentation");
    }
    Ok(())
}

fn configure_direct_surface(
    surface: &wgpu::Surface<'_>,
    device: &wgpu::Device,
    config: &wgpu::SurfaceConfiguration,
    failure_context: &'static str,
) -> Result<()> {
    let error_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    surface.configure(device, config);
    if let Some(error) = pollster::block_on(error_scope.pop()) {
        bail!("{failure_context}: {error}");
    }
    Ok(())
}

fn select_device<'a>(
    session: &mut LibSeatSession,
    devices: impl Iterator<Item = (Dev, &'a std::path::Path)>,
) -> Result<(Dev, PathBuf)> {
    let primary = primary_gpu(session.seat())?.context("no DRM GPU was found for the seat")?;
    let devices = devices
        .map(|(device_id, path)| (device_id, path.to_path_buf()))
        .collect::<Vec<_>>();
    devices
        .iter()
        .find(|(_, path)| *path == primary)
        .cloned()
        .or_else(|| devices.first().cloned())
        .context("udev reported no DRM devices for the seat")
}

fn select_output(drm: &DrmDeviceFd) -> Result<(connector::Info, Mode)> {
    let mut scanner: DrmScanner = DrmScanner::new();
    let (connector, _) = scanner
        .scan_connectors(drm)?
        .into_iter()
        .find_map(|event| match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } if !connector.modes().is_empty() => Some((connector, crtc)),
            _ => None,
        })
        .context("no connected DRM connector with a usable CRTC and mode")?;
    let mode = connector
        .modes()
        .iter()
        .copied()
        .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector.modes().first().copied())
        .context("DRM connector has no modes")?;
    Ok((connector, mode))
}

fn select_vulkan_adapter(instance: &wgpu::Instance, device_id: Dev) -> Result<wgpu::Adapter> {
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
    adapters
        .into_iter()
        .find(|adapter| adapter_matches_device(adapter, device_id))
        .context("no Vulkan adapter matches the selected DRM device")
}

fn adapter_matches_device(adapter: &wgpu::Adapter, device_id: Dev) -> bool {
    // SAFETY: the guard is only used to query immutable Vulkan adapter
    // properties and is dropped before the public wgpu adapter is used again.
    let Some(adapter) = (unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }) else {
        return false;
    };
    let mut drm_properties = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm_properties);
    unsafe {
        adapter
            .shared_instance()
            .raw_instance()
            .get_physical_device_properties2(adapter.raw_physical_device(), &mut properties);
    }
    let selected_major = i64::from(major(device_id));
    let selected_minor = i64::from(minor(device_id));
    (drm_properties.has_primary != 0
        && drm_properties.primary_major == selected_major
        && drm_properties.primary_minor == selected_minor)
        || (drm_properties.has_render != 0
            && drm_properties.render_major == selected_major
            && drm_properties.render_minor == selected_minor)
}

fn inspect_direct_mode(
    adapter: &wgpu::Adapter,
    fd: i32,
    connector_id: u32,
    drm_mode: Mode,
) -> Result<DirectMode> {
    // Vulkan exposes the connector's display, modes, and compatible planes
    // before acquisition. Leave the one initial acquisition to wgpu-hal;
    // acquiring here as well is rejected by RADV.
    let display = get_direct_display(adapter, fd, connector_id)?;
    let adapter = unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }
        .context("selected adapter is not backed by Vulkan")?;
    let shared = adapter.shared_instance();
    let physical_device = adapter.raw_physical_device();

    let display_api = khr::display::Instance::new(shared.entry(), shared.raw_instance());
    let modes = unsafe { display_api.get_display_mode_properties(physical_device, display) }
        .context("Vulkan could not enumerate display modes")?;
    for mode in &modes {
        info!(
            width = mode.parameters.visible_region.width,
            height = mode.parameters.visible_region.height,
            refresh_millihertz = mode.parameters.refresh_rate,
            "Vulkan DRM display mode"
        );
    }
    let target_size = drm_mode.size();
    let target_refresh = SmithayOutputMode::from(drm_mode).refresh;
    let selected_mode = modes
        .iter()
        .filter(|mode| {
            mode.parameters.visible_region.width == u32::from(target_size.0)
                && mode.parameters.visible_region.height == u32::from(target_size.1)
        })
        .min_by_key(|mode| {
            i64::from(mode.parameters.refresh_rate).abs_diff(i64::from(target_refresh))
        })
        .context("Vulkan did not expose the selected DRM mode dimensions")?;
    let refresh_delta =
        i64::from(selected_mode.parameters.refresh_rate).abs_diff(i64::from(target_refresh));
    info!(
        smithay_refresh_millihertz = target_refresh,
        vulkan_refresh_millihertz = selected_mode.parameters.refresh_rate,
        refresh_delta_millihertz = refresh_delta,
        "matched Smithay mode to Vulkan display mode"
    );
    if refresh_delta > 1_000 {
        bail!("closest Vulkan refresh differs from the selected DRM mode by {refresh_delta} mHz");
    }

    let planes =
        unsafe { display_api.get_physical_device_display_plane_properties(physical_device) }
            .context("Vulkan could not enumerate display planes")?;
    let plane_index = planes
        .iter()
        .enumerate()
        .find_map(|(index, plane)| {
            let index = u32::try_from(index).ok()?;
            let supported =
                unsafe { display_api.get_display_plane_supported_displays(physical_device, index) }
                    .ok()?;
            let available =
                plane.current_display == vk::DisplayKHR::null() || plane.current_display == display;
            (available && supported.contains(&display)).then_some(index)
        })
        .context("Vulkan exposed no display plane compatible with the DRM connector")?;

    Ok(DirectMode {
        plane_index,
        connector_id,
        width: selected_mode.parameters.visible_region.width,
        height: selected_mode.parameters.visible_region.height,
        refresh_millihertz: selected_mode.parameters.refresh_rate,
    })
}

fn get_direct_display(
    adapter: &wgpu::Adapter,
    fd: i32,
    connector_id: u32,
) -> Result<vk::DisplayKHR> {
    // SAFETY: this guard is used only for immutable extension discovery and a
    // DRM-connector-to-Vulkan-display query against the live fd.
    let adapter = unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }
        .context("selected adapter is not backed by Vulkan")?;
    let shared = adapter.shared_instance();
    if !shared
        .extensions()
        .contains(&ext::acquire_drm_display::NAME)
    {
        bail!("Vulkan driver does not support VK_EXT_acquire_drm_display");
    }
    let physical_device = adapter.raw_physical_device();
    let acquire = ext::acquire_drm_display::Instance::new(shared.entry(), shared.raw_instance());
    let display = unsafe { acquire.get_drm_display(physical_device, fd, connector_id) }
        .context("Vulkan could not map the DRM connector to a display")?;
    Ok(display)
}

fn ensure_same_adapter(expected: &wgpu::Adapter, actual: &wgpu::Adapter) -> Result<()> {
    // SAFETY: the guards are used only to compare immutable Vulkan physical
    // device handles belonging to the same live wgpu instance.
    let expected = unsafe { expected.as_hal::<wgpu::hal::api::Vulkan>() }
        .context("preflight adapter is not backed by Vulkan")?;
    let actual = unsafe { actual.as_hal::<wgpu::hal::api::Vulkan>() }
        .context("surface-compatible adapter is not backed by Vulkan")?;
    if expected.raw_physical_device() != actual.raw_physical_device() {
        bail!("the DRM surface selected a different Vulkan physical device than preflight");
    }
    Ok(())
}

fn present_clear_frame(
    surface: &wgpu::Surface<'_>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    frame: u64,
) -> Result<PresentOutcome> {
    use wgpu::CurrentSurfaceTexture;

    let acquire_started_at = Instant::now();
    let current_texture = surface.get_current_texture();
    let acquire_elapsed = acquire_started_at.elapsed();
    if acquire_elapsed >= SLOW_ACQUIRE_THRESHOLD {
        warn!(
            elapsed_milliseconds = acquire_elapsed.as_millis(),
            "direct DRM surface acquisition was slow"
        );
    }
    let (surface_texture, suboptimal) = match current_texture {
        CurrentSurfaceTexture::Success(texture) => (texture, false),
        CurrentSurfaceTexture::Suboptimal(texture) => {
            warn!("direct DRM surface is suboptimal");
            (texture, true)
        }
        CurrentSurfaceTexture::Timeout => return Ok(PresentOutcome::Deferred("timeout")),
        CurrentSurfaceTexture::Occluded => return Ok(PresentOutcome::Deferred("occluded")),
        CurrentSurfaceTexture::Outdated => return Ok(PresentOutcome::Reconfigure("outdated")),
        CurrentSurfaceTexture::Lost => return Ok(PresentOutcome::Lost),
        CurrentSurfaceTexture::Validation => {
            bail!("wgpu reported a validation error while acquiring the DRM surface")
        }
    };
    let view = surface_texture
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("weld direct DRM probe encoder"),
    });
    {
        let phase = (frame % 240) as f64 / 240.0;
        let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("weld direct DRM probe pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.05 + phase * 0.2,
                        g: 0.12,
                        b: 0.22 + (1.0 - phase) * 0.2,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
    }
    queue.submit([encoder.finish()]);
    queue.present(surface_texture);
    if suboptimal {
        Ok(PresentOutcome::PresentedSuboptimal)
    } else {
        Ok(PresentOutcome::Presented)
    }
}
