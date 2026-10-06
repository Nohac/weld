//! Foreign ownership transitions shared by local and offscreen sampling.

use anyhow::{Context, Result};
use ash::vk;

pub(crate) fn sampling_barrier_command(
    device: &wgpu::Device,
    raw_device: &ash::Device,
    queue_family: u32,
    images: &[vk::Image],
    acquire: bool,
) -> Result<Option<wgpu::CommandBuffer>> {
    if images.is_empty() {
        return Ok(None);
    }
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some(if acquire {
            "weld DMA-BUF foreign acquire"
        } else {
            "weld DMA-BUF foreign release"
        }),
    });
    let recorded =
        // SAFETY: the callback records one Vulkan barrier into wgpu's
        // active command buffer without ending or submitting it. The image
        // belongs to this device and is retained through GPU completion.
        unsafe {
            encoder.as_hal_mut::<wgpu::hal::api::Vulkan, _, _>(|raw_encoder| {
                let raw_encoder = raw_encoder?;
                let (old_layout, new_layout, source_family, destination_family) = if acquire {
                    (
                        vk::ImageLayout::GENERAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::QUEUE_FAMILY_FOREIGN_EXT,
                        queue_family,
                    )
                } else {
                    (
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::ImageLayout::GENERAL,
                        queue_family,
                        vk::QUEUE_FAMILY_FOREIGN_EXT,
                    )
                };
                let barriers = images
                    .iter()
                    .map(|&image| {
                        vk::ImageMemoryBarrier::default()
                            .src_access_mask(if acquire {
                                vk::AccessFlags::MEMORY_WRITE
                            } else {
                                vk::AccessFlags::SHADER_READ
                            })
                            .dst_access_mask(if acquire {
                                vk::AccessFlags::SHADER_READ
                            } else {
                                vk::AccessFlags::empty()
                            })
                            .old_layout(old_layout)
                            .new_layout(new_layout)
                            .src_queue_family_index(source_family)
                            .dst_queue_family_index(destination_family)
                            .image(image)
                            .subresource_range(vk::ImageSubresourceRange {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                base_mip_level: 0,
                                level_count: 1,
                                base_array_layer: 0,
                                layer_count: 1,
                            })
                    })
                    .collect::<Vec<_>>();
                raw_device.cmd_pipeline_barrier(
                    raw_encoder.raw_handle(),
                    if acquire {
                        vk::PipelineStageFlags::ALL_COMMANDS
                    } else {
                        vk::PipelineStageFlags::FRAGMENT_SHADER
                    },
                    if acquire {
                        vk::PipelineStageFlags::FRAGMENT_SHADER
                    } else {
                        vk::PipelineStageFlags::BOTTOM_OF_PIPE
                    },
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &barriers,
                );
                Some(())
            })
        };
    recorded.context("wgpu command encoder is not backed by Vulkan")?;
    Ok(Some(encoder.finish()))
}
