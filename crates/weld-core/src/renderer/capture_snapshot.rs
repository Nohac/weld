//! Preserve a completed output before its display allocation is reused.
use crate::host::{CompositionFrame, CompositionTargetView};
use anyhow::{Result, ensure};

pub(crate) fn snapshot_composition(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    frame: &CompositionFrame,
    previous: Option<CompositionFrame>,
) -> Result<CompositionFrame> {
    let target = frame.target();
    if let Some(texture) = frame.owned_texture() {
        return Ok(CompositionFrame::owned(target.clone(), texture.clone()));
    }
    let source = target.view().texture();
    ensure!(
        source.usage().contains(wgpu::TextureUsages::COPY_SRC),
        "output cannot be copied for capture"
    );
    let extent = target.extent();
    let snapshot = previous
        .filter(|previous| {
            previous.target().extent() == extent && previous.target().format() == target.format()
        })
        .unwrap_or_else(|| {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("retained output capture"),
                size: source.size(),
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: target.format(),
                usage: wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            CompositionFrame::owned(
                CompositionTargetView::new(
                    texture.create_view(&Default::default()),
                    extent,
                    target.format(),
                ),
                texture,
            )
        });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("retain completed output"),
    });
    encoder.copy_texture_to_texture(
        source.as_image_copy(),
        snapshot.target().view().texture().as_image_copy(),
        source.size(),
    );
    // The physical renderer's foreign release barrier expects COLOR_TARGET.
    // Restore wgpu's tracked state before its overlay and release submissions.
    encoder.transition_resources(
        std::iter::empty(),
        std::iter::once(wgpu::TextureTransition {
            texture: source,
            selector: None,
            state: wgpu::TextureUses::COLOR_TARGET,
        }),
    );
    queue.submit([encoder.finish()]);
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{renderer::read_owned_frame_rgba, surface::Extent};

    #[test]
    #[ignore = "requires a Vulkan device"]
    fn capture_snapshot_survives_display_target_reuse() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter =
            pollster::block_on(instance.request_adapter(&Default::default())).expect("adapter");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&Default::default())).expect("device");
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("display reuse test"),
            size: wgpu::Extent3d {
                width: 7,
                height: 5,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            usage: wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let frame = CompositionFrame::external(CompositionTargetView::new(
            texture.create_view(&Default::default()),
            Extent::new(7, 5),
            texture.format(),
        ));
        let write = |color: [u8; 4]| {
            queue.write_texture(
                texture.as_image_copy(),
                &color.repeat(35),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(28),
                    rows_per_image: Some(5),
                },
                texture.size(),
            )
        };
        write([0, 0, 255, 255]);
        let red = snapshot_composition(&device, &queue, &frame, None).expect("red snapshot");
        write([255, 0, 0, 255]);
        let blue = snapshot_composition(&device, &queue, &frame, None).expect("blue snapshot");
        assert_eq!(
            read_owned_frame_rgba(&device, &queue, &red).expect("red pixels"),
            [255, 0, 0, 255].repeat(35)
        );
        assert_eq!(
            read_owned_frame_rgba(&device, &queue, &blue).expect("blue pixels"),
            [0, 0, 255, 255].repeat(35)
        );
        let old_texture = red.target().view().texture().clone();
        let reused = snapshot_composition(&device, &queue, &frame, Some(red)).expect("reuse");
        assert_eq!(reused.target().view().texture(), &old_texture);
        assert_eq!(
            read_owned_frame_rgba(&device, &queue, &reused).expect("reused pixels"),
            [0, 0, 255, 255].repeat(35)
        );
    }
}
