//! Probe-validated creation of a direct Vulkan display surface.

use std::os::fd::AsRawFd;

use anyhow::{Context, Result, anyhow, bail};
use ash::{ext, khr, vk};
use smithay::{
    backend::drm::DrmDeviceFd,
    output::Mode as SmithayOutputMode,
    reexports::{
        drm::control::{Mode, connector},
        rustix::fs::{Dev, major, minor},
    },
};
use tracing::info;

#[derive(Clone, Copy, Debug)]
pub(super) struct DirectMode {
    pub(super) plane_index: u32,
    pub(super) connector_id: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) refresh_millihertz: u32,
}

pub(super) struct DirectDrmGpu {
    pub(super) surface: wgpu::Surface<'static>,
    pub(super) queue: wgpu::Queue,
    pub(super) device: wgpu::Device,
    pub(super) adapter: wgpu::Adapter,
    pub(super) instance: wgpu::Instance,
    pub(super) surface_config: wgpu::SurfaceConfiguration,
    pub(super) mode: DirectMode,
    pub(super) _drm: DrmDeviceFd,
}

impl DirectDrmGpu {
    pub(super) fn new(
        drm: &DrmDeviceFd,
        device_id: Dev,
        device_path: &std::path::Path,
        connector: &connector::Info,
        mode: Mode,
    ) -> Result<Self> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        descriptor.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(descriptor);
        let preflight_adapter = select_vulkan_adapter(&instance, device_id)?;
        let direct_mode = inspect_direct_mode(
            &preflight_adapter,
            drm.as_raw_fd(),
            u32::from(connector.handle()),
            mode,
        )?;
        let connector_name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );
        info!(
            node = %device_path.display(),
            connector = %connector_name,
            plane_index = direct_mode.plane_index,
            width = direct_mode.width,
            height = direct_mode.height,
            vulkan_refresh_millihertz = direct_mode.refresh_millihertz,
            adapter = ?preflight_adapter.get_info(),
            "validated direct DRM display parameters"
        );

        // Keep this preflight and its structured fields synchronized with
        // examples/drm_wsi_probe.rs. The probe remains the isolated diagnostic
        // when production initialization fails.
        // SAFETY: libseat keeps the DRM fd alive, discovery selected the
        // connector and mode from it, and inspect_direct_mode selected a plane
        // that advertises support for the exact Vulkan display.
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
        .context("wgpu could not create the direct DRM surface; run scripts/run-drm-wsi-probe")?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .context("no Vulkan adapter can present to the direct DRM surface")?;
        ensure_same_adapter(&preflight_adapter, &adapter)?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("weld direct DRM device"),
            ..Default::default()
        }))
        .context("failed to create the direct DRM wgpu device")?;

        let capabilities = surface.get_capabilities(&adapter);
        let mut surface_config = surface
            .get_default_config(&adapter, direct_mode.width, direct_mode.height)
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
        surface_config.desired_maximum_frame_latency = 3;
        info!(
            formats = ?capabilities.formats,
            present_modes = ?capabilities.present_modes,
            alpha_modes = ?capabilities.alpha_modes,
            selected_format = ?surface_config.format,
            selected_present_mode = ?surface_config.present_mode,
            "prepared direct DRM surface configuration"
        );

        Ok(Self {
            surface,
            queue,
            device,
            adapter,
            instance,
            surface_config,
            mode: direct_mode,
            _drm: drm.clone(),
        })
    }
}

fn select_vulkan_adapter(instance: &wgpu::Instance, device_id: Dev) -> Result<wgpu::Adapter> {
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
    adapters
        .into_iter()
        .find(|adapter| adapter_matches_device(adapter, device_id))
        .context("no Vulkan adapter matches the selected DRM device")
}

fn adapter_matches_device(adapter: &wgpu::Adapter, device_id: Dev) -> bool {
    // SAFETY: this guard only queries immutable Vulkan adapter properties.
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
    let display = get_direct_display(adapter, fd, connector_id)?;
    let adapter = unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }
        .context("selected adapter is not backed by Vulkan")?;
    let shared = adapter.shared_instance();
    let physical_device = adapter.raw_physical_device();
    let display_api = khr::display::Instance::new(shared.entry(), shared.raw_instance());
    let modes = unsafe { display_api.get_display_mode_properties(physical_device, display) }
        .context("Vulkan could not enumerate display modes")?;
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
    unsafe { acquire.get_drm_display(physical_device, fd, connector_id) }
        .context("Vulkan could not map the DRM connector to a display")
}

fn ensure_same_adapter(expected: &wgpu::Adapter, actual: &wgpu::Adapter) -> Result<()> {
    let expected = unsafe { expected.as_hal::<wgpu::hal::api::Vulkan>() }
        .context("preflight adapter is not backed by Vulkan")?;
    let actual = unsafe { actual.as_hal::<wgpu::hal::api::Vulkan>() }
        .context("surface-compatible adapter is not backed by Vulkan")?;
    if expected.raw_physical_device() != actual.raw_physical_device() {
        bail!("the DRM surface selected a different Vulkan physical device than preflight");
    }
    Ok(())
}
