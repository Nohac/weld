//! Vulkan device selection for GBM/KMS composition and client DMA-BUF import.

use anyhow::{Context, Result, bail};
use ash::vk;
use smithay::{
    backend::allocator::Format,
    reexports::rustix::fs::{Dev, major, minor},
};
use tracing::info;

use crate::dmabuf::{DmabufCapabilities, renderable_scanout_formats, request_weld_device};

pub(super) struct DrmGpu {
    pub(super) queue: wgpu::Queue,
    pub(super) device: wgpu::Device,
    pub(super) renderable_scanout_formats: Vec<Format>,
    pub(super) dmabuf_capabilities: Option<DmabufCapabilities>,
    pub(super) adapter: wgpu::Adapter,
    pub(super) instance: wgpu::Instance,
}

impl DrmGpu {
    pub(super) fn new(device_id: Dev, device_path: &std::path::Path) -> Result<Self> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        descriptor.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(descriptor);
        let adapter = select_vulkan_adapter(&instance, device_id)?;
        let renderable_scanout_formats = renderable_scanout_formats(&adapter)?;
        if renderable_scanout_formats.is_empty() {
            bail!("selected Vulkan adapter exposes no importable sRGB scanout modifier");
        }
        let (device, queue, dmabuf_capabilities) =
            request_weld_device(&adapter, "weld GBM/KMS device")
                .context("failed to create the DRM wgpu device")?;
        info!(
            node = %device_path.display(),
            adapter = ?adapter.get_info(),
            scanout_format_modifier_pairs = renderable_scanout_formats.len(),
            "prepared Vulkan device for GBM/KMS presentation"
        );
        Ok(Self {
            queue,
            device,
            renderable_scanout_formats,
            dmabuf_capabilities,
            adapter,
            instance,
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
    // SAFETY: both handles came from this adapter and the output chain remains
    // alive for the duration of the immutable query.
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
