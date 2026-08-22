//! Vulkan interoperability required at Smithay's leased scanout boundary.

use anyhow::{Context, Result};
use ash::vk;
use smithay::{
    backend::allocator::{Format, Fourcc, Modifier},
    reexports::rustix::fs::{Dev, major, minor},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ForeignImageBarrier {
    Acquire,
    Release,
}

pub(super) fn adapter_matches_device(adapter: &wgpu::Adapter, device_id: Dev) -> bool {
    // SAFETY: this guard only queries immutable Vulkan adapter properties.
    let Some(adapter) = (unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }) else {
        return false;
    };
    let mut drm_properties = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm_properties);
    // SAFETY: the handles and output chain belong to this live adapter.
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

pub(super) fn renderable_scanout_formats(adapter: &wgpu::Adapter) -> Result<Vec<Format>> {
    let raw_adapter = unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }
        .context("scanout adapter is not backed by Vulkan")?;
    let instance = raw_adapter.shared_instance().raw_instance();
    let physical_device = raw_adapter.raw_physical_device();
    Ok(modifiers_for_usage(
        instance,
        physical_device,
        vk::Format::B8G8R8A8_SRGB,
        vk::FormatFeatureFlags::COLOR_ATTACHMENT,
        vk::ImageUsageFlags::COLOR_ATTACHMENT,
    )?
    .into_iter()
    .map(|modifier| Format {
        code: Fourcc::Argb8888,
        modifier: Modifier::from(modifier),
    })
    .collect())
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
        // SAFETY: the output chain remains live for this immutable query.
        unsafe {
            instance.get_physical_device_format_properties2(
                physical_device,
                format,
                &mut properties,
            );
        }
        list.drm_format_modifier_count as usize
    };
    let mut entries = Vec::<vk::DrmFormatModifierPropertiesEXT>::with_capacity(count);
    let mut list = vk::DrmFormatModifierPropertiesListEXT {
        drm_format_modifier_count: count as u32,
        p_drm_format_modifier_properties: entries.as_mut_ptr(),
        ..Default::default()
    };
    let mut properties = vk::FormatProperties2::default().push_next(&mut list);
    // SAFETY: `entries` has capacity for the count returned by the first query;
    // Vulkan initializes at most that many records.
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
    // SAFETY: every structure in both pNext chains remains live for this query.
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

/// Records one Vulkan foreign-queue ownership transition in a wgpu command.
///
/// # Safety
///
/// `image` must belong to `raw_device`, remain alive through command completion,
/// and be imported into `device`. `queue_family` must be the family used by
/// `device`, and callers must submit acquire, render, and release in that order.
pub(super) unsafe fn foreign_image_barrier_command(
    device: &wgpu::Device,
    raw_device: &ash::Device,
    queue_family: u32,
    image: vk::Image,
    previously_used: bool,
    direction: ForeignImageBarrier,
) -> Result<wgpu::CommandBuffer> {
    let acquire = direction == ForeignImageBarrier::Acquire;
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some(if acquire {
            "Weld DRM scanout acquire"
        } else {
            "Weld DRM scanout release"
        }),
    });
    // SAFETY: the caller upholds the image, device, queue-family, and lifetime
    // invariants documented above. This encoder contains only the barrier.
    let recorded = unsafe {
        encoder.as_hal_mut::<wgpu::hal::api::Vulkan, _, _>(|raw_encoder| {
            let raw_encoder = raw_encoder?;
            let first_acquire = acquire && !previously_used;
            let barrier = vk::ImageMemoryBarrier::default()
                .src_access_mask(if acquire {
                    if first_acquire {
                        vk::AccessFlags::empty()
                    } else {
                        vk::AccessFlags::MEMORY_READ
                    }
                } else {
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE
                })
                .dst_access_mask(if acquire {
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE
                } else {
                    vk::AccessFlags::MEMORY_READ
                })
                .old_layout(if first_acquire {
                    vk::ImageLayout::UNDEFINED
                } else if acquire {
                    vk::ImageLayout::GENERAL
                } else {
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
                })
                .new_layout(if acquire {
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
                } else {
                    vk::ImageLayout::GENERAL
                })
                .src_queue_family_index(if first_acquire {
                    vk::QUEUE_FAMILY_IGNORED
                } else if acquire {
                    vk::QUEUE_FAMILY_FOREIGN_EXT
                } else {
                    queue_family
                })
                .dst_queue_family_index(if first_acquire {
                    vk::QUEUE_FAMILY_IGNORED
                } else if acquire {
                    queue_family
                } else {
                    vk::QUEUE_FAMILY_FOREIGN_EXT
                })
                .image(image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });
            raw_device.cmd_pipeline_barrier(
                raw_encoder.raw_handle(),
                if acquire {
                    vk::PipelineStageFlags::TOP_OF_PIPE
                } else {
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                },
                if acquire {
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                } else {
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE
                },
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
            Some(())
        })
    };
    recorded.context("wgpu did not expose a Vulkan encoder for scanout ownership")?;
    Ok(encoder.finish())
}
