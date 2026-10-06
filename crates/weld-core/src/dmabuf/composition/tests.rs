use super::*;
use crate::{dmabuf::WaylandShmBuffer, renderer::CompositionBlitter};
use std::sync::mpsc;
use weld_client::{
    ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, ClientSourceId, CompositionLayer,
    LogicalPoint, LogicalSize, SurfaceContentView, SurfaceLayerId, SurfaceLayerPlacement,
};

#[test]
fn composition_shader_validates() {
    let module = wgpu::naga::front::wgsl::parse_str(include_str!("../composition.wgsl"))
        .expect("shader syntax");
    wgpu::naga::valid::Validator::new(
        wgpu::naga::valid::ValidationFlags::all(),
        wgpu::naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .expect("shader validation");
}

fn layer(
    id: u64,
    color: [u8; 4],
    position: LogicalPoint,
    size: LogicalSize,
    opaque: bool,
) -> CompositionLayer {
    let source = ClientSourceId::new(1);
    let buffer = ClientBufferLease::new(
        ClientBufferId::new(source, id),
        ClientBufferUseId::new(source, id),
        ClientBufferMetadata::new(Extent::new(2, 2), opaque),
        Rc::new(DirectClientBufferAccess::Shm(WaylandShmBuffer {
            bgra_pixels: color.repeat(4),
        })),
        |_| {},
    )
    .expect("lease");
    CompositionLayer {
        buffer,
        placement: SurfaceLayerPlacement {
            layer: SurfaceLayerId::new(id),
            position,
            view: SurfaceContentView {
                source_x: 0.0,
                source_y: 0.0,
                source_width: 2.0,
                source_height: 2.0,
                logical_width: size.width,
                logical_height: size.height,
            },
        },
    }
}

#[test]
#[ignore = "requires WELD_TEST_RENDER_NODE and a Vulkan DMA-BUF device"]
fn gpu_composes_alpha_and_clipping_and_keeps_leased_targets_immutable() -> Result<()> {
    let path = std::env::var_os("WELD_TEST_RENDER_NODE").context("set WELD_TEST_RENDER_NODE")?;
    let mut compositor = SourceCompositor::new(Path::new(&path))?;
    let mut frame = ComposedBuffer {
        extent: Extent::new(64, 64),
        logical_size: LogicalSize::new(32.0, 32.0),
        layers: vec![
            layer(
                1,
                [200, 0, 0, 0],
                LogicalPoint::ZERO,
                LogicalSize::new(32.0, 32.0),
                true,
            ),
            layer(
                2,
                [0, 0, 100, 128],
                LogicalPoint::new(16.0, 16.0),
                LogicalSize::new(32.0, 32.0),
                false,
            ),
        ],
    };
    let first = compositor.compose(&frame)?;
    frame.layers.pop();
    let second = compositor.compose(&frame)?;
    assert_eq!(
        compositor.textures.len(),
        2,
        "retain recent buffer rotation imports"
    );
    assert!(!Rc::ptr_eq(&first.target, &second.target));
    let pixels = readback(&compositor, &first)?;
    assert_eq!(&pixels[0..4], &[200, 0, 0, 255]);
    let overlap = (48 * 64 + 48) * 4;
    for (actual, expected) in pixels[overlap..overlap + 4]
        .iter()
        .zip([100_u8, 0, 100, 255])
    {
        assert!(
            actual.abs_diff(expected) <= 1,
            "alpha pixel: {actual} != {expected}"
        );
    }
    let pixels = readback(&compositor, &second)?;
    assert_eq!(&pixels[overlap..overlap + 4], &[200, 0, 0, 255]);
    drop(first);
    let third = compositor.compose(&frame)?;
    assert_eq!(third.export()?.extent, Extent::new(64, 64));
    assert_eq!(compositor.targets.len(), 2, "reuse released target storage");
    Ok(())
}

fn readback(compositor: &SourceCompositor, image: &ComposedImage) -> Result<Vec<u8>> {
    let source = compositor.sources.import(&image.target.dmabuf)?;
    let blitter = CompositionBlitter::new(&compositor.device, FORMAT);
    let binding = blitter.create_bind_group(&compositor.device, "probe", &source.view);
    let texture = compositor.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe readback"),
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let buffer = compositor.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe readback"),
        size: 64 * 64 * 4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let acquire = sampling_barrier_command(
        &compositor.device,
        &compositor.raw_device,
        compositor.queue_family,
        &[source.image],
        true,
    )?;
    let release = sampling_barrier_command(
        &compositor.device,
        &compositor.raw_device,
        compositor.queue_family,
        &[source.image],
        false,
    )?;
    let mut encoder = compositor
        .device
        .create_command_encoder(&Default::default());
    blitter.encode(
        &mut encoder,
        "probe",
        &texture.create_view(&Default::default()),
        &binding,
    );
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: Some(64),
            },
        },
        texture.size(),
    );
    compositor
        .queue
        .submit(acquire.into_iter().chain([encoder.finish()]).chain(release));
    let (sender, receiver) = mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
    compositor
        .device
        .poll(wgpu::PollType::wait_indefinitely())?;
    receiver.recv()??;
    let pixels = buffer.slice(..).get_mapped_range()?.to_vec();
    buffer.unmap();
    compositor.sources.remove(&image.target.dmabuf);
    Ok(pixels)
}
