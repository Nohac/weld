//! Vulkan device creation and exact linux-dmabuf feedback capabilities.

use anyhow::{Context, Result};
use ash::{ext, vk};
use smithay::{
    backend::allocator::{Format, Fourcc, Modifier},
    reexports::rustix::fs::{Dev, makedev},
};
use tracing::{info, warn};

const IMPORT_FORMATS: [(Fourcc, vk::Format); 4] = [
    (Fourcc::Argb8888, vk::Format::B8G8R8A8_UNORM),
    (Fourcc::Xrgb8888, vk::Format::B8G8R8A8_UNORM),
    (Fourcc::Abgr8888, vk::Format::R8G8B8A8_UNORM),
    (Fourcc::Xbgr8888, vk::Format::R8G8B8A8_UNORM),
];

#[cfg(feature = "profiling-tracy")]
const PROFILING_FEATURES: wgpu::Features = wgpu::Features::TIMESTAMP_QUERY
    .union(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS)
    .union(wgpu::Features::TIMESTAMP_QUERY_INSIDE_PASSES);

/// The exact device and format/modifier pairs Weld can import through wgpu.
#[derive(Clone, Debug)]
pub struct DmabufCapabilities {
    pub(crate) main_device: Dev,
    pub(crate) formats: Vec<Format>,
}

pub fn request_weld_device(
    adapter: &wgpu::Adapter,
    label: &'static str,
) -> Result<(wgpu::Device, wgpu::Queue, Option<DmabufCapabilities>)> {
    let Some(capabilities) = discover_capabilities(adapter)? else {
        warn!("linux-dmabuf unavailable on the selected Vulkan adapter; retaining the SHM path");
        let descriptor = wgpu::DeviceDescriptor {
            label: Some(label),
            required_features: required_device_features(adapter, wgpu::Features::empty()),
            ..Default::default()
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&descriptor))
            .context("failed to create the wgpu device")?;
        return Ok((device, queue, None));
    };

    let descriptor = wgpu::DeviceDescriptor {
        label: Some(label),
        required_features: required_device_features(
            adapter,
            wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF,
        ),
        ..Default::default()
    };
    // SAFETY: capability discovery verified the Vulkan backend, wgpu's
    // external-memory feature, and VK_EXT_queue_family_foreign. The callback
    // only appends that supported extension and otherwise preserves wgpu's
    // requested device configuration. The same descriptor is supplied to the
    // HAL open and wgpu wrapping operations.
    let (device, queue) = unsafe {
        let raw_adapter = adapter
            .as_hal::<wgpu::hal::api::Vulkan>()
            .context("selected adapter stopped exposing its Vulkan HAL")?;
        let open_device = raw_adapter.open_with_callback(
            descriptor.required_features,
            &descriptor.required_limits,
            &descriptor.memory_hints,
            Some(Box::new(|arguments| {
                if !arguments
                    .extensions
                    .contains(&ext::queue_family_foreign::NAME)
                {
                    arguments.extensions.push(ext::queue_family_foreign::NAME);
                }
            })),
        )?;
        adapter.create_device_from_hal::<wgpu::hal::api::Vulkan>(open_device, &descriptor)?
    };
    info!(
        format_modifier_pairs = capabilities.formats.len(),
        "enabled direct linux-dmabuf sampling"
    );
    Ok((device, queue, Some(capabilities)))
}

/// Exact single-plane DRM formats that the selected Vulkan adapter can import
/// as sRGB color attachments for the physical scanout blit.
pub(crate) fn renderable_scanout_formats(adapter: &wgpu::Adapter) -> Result<Vec<Format>> {
    let raw_adapter = unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }
        .context("scanout adapter is not backed by Vulkan")?;
    let instance = raw_adapter.shared_instance().raw_instance();
    let physical_device = raw_adapter.raw_physical_device();
    let modifiers = modifiers_for_usage(
        instance,
        physical_device,
        vk::Format::B8G8R8A8_SRGB,
        vk::FormatFeatureFlags::COLOR_ATTACHMENT,
        vk::ImageUsageFlags::COLOR_ATTACHMENT,
    )?;
    Ok(modifiers
        .into_iter()
        .map(|modifier| Format {
            code: Fourcc::Argb8888,
            modifier: Modifier::from(modifier),
        })
        .collect())
}

fn required_device_features(adapter: &wgpu::Adapter, required: wgpu::Features) -> wgpu::Features {
    #[cfg(feature = "profiling-tracy")]
    let required = required.union(adapter.features().intersection(PROFILING_FEATURES));
    #[cfg(not(feature = "profiling-tracy"))]
    let _ = adapter;
    required
}

fn discover_capabilities(adapter: &wgpu::Adapter) -> Result<Option<DmabufCapabilities>> {
    if !adapter
        .features()
        .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
    {
        return Ok(None);
    }
    // SAFETY: this guard is used only for immutable Vulkan capability queries.
    let Some(raw_adapter) = (unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }) else {
        return Ok(None);
    };
    let instance = raw_adapter.shared_instance().raw_instance();
    let physical_device = raw_adapter.raw_physical_device();
    // SAFETY: `physical_device` belongs to `instance`; the returned extension
    // names are driver-owned values copied by ash.
    let extensions = unsafe { instance.enumerate_device_extension_properties(physical_device) }
        .context("could not enumerate Vulkan device extensions")?;
    let has_extension = |name: &std::ffi::CStr| {
        extensions.iter().any(|property| {
            // SAFETY: Vulkan guarantees a null-terminated extension name.
            let property_name =
                unsafe { std::ffi::CStr::from_ptr(property.extension_name.as_ptr()) };
            property_name == name
        })
    };
    if !has_extension(ext::queue_family_foreign::NAME) {
        return Ok(None);
    }

    let Some(main_device) = render_node(instance, physical_device)? else {
        return Ok(None);
    };
    let mut formats = Vec::new();
    for (fourcc, vulkan_format) in IMPORT_FORMATS {
        for modifier in sampleable_modifiers(instance, physical_device, vulkan_format)? {
            formats.push(Format {
                code: fourcc,
                modifier: Modifier::from(modifier),
            });
        }
    }
    formats.sort_unstable_by_key(|format| (format.code as u32, u64::from(format.modifier)));
    formats.dedup();
    if formats.is_empty() {
        return Ok(None);
    }
    Ok(Some(DmabufCapabilities {
        main_device,
        formats,
    }))
}

fn render_node(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Result<Option<Dev>> {
    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
    // SAFETY: both handles are paired and the output chain remains valid for
    // the duration of the immutable property query.
    unsafe { instance.get_physical_device_properties2(physical_device, &mut properties) };
    if drm.has_render == 0 || drm.render_major < 0 || drm.render_minor < 0 {
        return Ok(None);
    }
    let major = u32::try_from(drm.render_major).context("DRM render-node major is invalid")?;
    let minor = u32::try_from(drm.render_minor).context("DRM render-node minor is invalid")?;
    Ok(Some(makedev(major, minor)))
}

fn sampleable_modifiers(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    format: vk::Format,
) -> Result<Vec<u64>> {
    modifiers_for_usage(
        instance,
        physical_device,
        format,
        vk::FormatFeatureFlags::SAMPLED_IMAGE,
        vk::ImageUsageFlags::SAMPLED,
    )
}

fn modifiers_for_usage(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    format: vk::Format,
    required_features: vk::FormatFeatureFlags,
    usage: vk::ImageUsageFlags,
) -> Result<Vec<u64>> {
    let count = {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut properties = vk::FormatProperties2::default().push_next(&mut list);
        // SAFETY: the output chain is live and belongs to this call.
        unsafe {
            instance.get_physical_device_format_properties2(
                physical_device,
                format,
                &mut properties,
            )
        };
        list.drm_format_modifier_count as usize
    };
    let mut entries = Vec::<vk::DrmFormatModifierPropertiesEXT>::with_capacity(count);
    let mut list = vk::DrmFormatModifierPropertiesListEXT {
        drm_format_modifier_count: count as u32,
        p_drm_format_modifier_properties: entries.as_mut_ptr(),
        ..Default::default()
    };
    let mut properties = vk::FormatProperties2::default().push_next(&mut list);
    // SAFETY: `entries` has capacity for the count returned by the preceding
    // query; Vulkan initializes at most that many entries.
    unsafe {
        instance.get_physical_device_format_properties2(physical_device, format, &mut properties);
        entries.set_len(list.drm_format_modifier_count as usize);
    }
    Ok(entries
        .into_iter()
        .filter(|entry| entry.drm_format_modifier_plane_count == 1)
        .filter(|entry| {
            entry
                .drm_format_modifier_tiling_features
                .contains(required_features)
        })
        .filter(|entry| {
            modifier_is_importable(
                instance,
                physical_device,
                format,
                entry.drm_format_modifier,
                usage,
            )
        })
        .map(|entry| entry.drm_format_modifier)
        .collect())
}

fn modifier_is_importable(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    format: vk::Format,
    modifier: u64,
    usage: vk::ImageUsageFlags,
) -> bool {
    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let image_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage)
        .push_next(&mut modifier_info)
        .push_next(&mut external_info);
    let mut external_properties = vk::ExternalImageFormatProperties::default();
    let mut image_properties =
        vk::ImageFormatProperties2::default().push_next(&mut external_properties);
    // SAFETY: every input/output structure in both pNext chains remains live
    // for this immutable capability query.
    let supported = unsafe {
        instance.get_physical_device_image_format_properties2(
            physical_device,
            &image_info,
            &mut image_properties,
        )
    }
    .is_ok();
    supported
        && external_properties
            .external_memory_properties
            .external_memory_features
            .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
}
