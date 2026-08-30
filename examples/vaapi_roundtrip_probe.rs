use std::collections::HashSet;

use anyhow::{Context, Result, ensure};
use weld_client::{
    ClientBufferId, ClientBufferMetadata, ClientBufferUseId, ClientId, ClientSourceId,
    ClientSurfaceId, Extent, SurfaceLayerId,
};
use weld_core::dmabuf::{
    DmabufContext, ExternalDmabuf, ExternalDmabufPlane, ImportedImageRegistry, PromotionImage,
    request_weld_device,
};
use weld_media::{MediaFrameId, MediaStreamId, StreamGeneration};
use weld_media_vaapi::{
    H264Encoder, VaapiDmabuf, VppConverter, VppOutput, create_xrgb_probe_frame, decode_h264_frame,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 192;
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

fn main() -> Result<()> {
    let mut instance_descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    instance_descriptor.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(instance_descriptor);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .context("no Vulkan adapter is available")?;
    let adapter_info = adapter.get_info();
    let (device, queue, capabilities) =
        request_weld_device(&adapter, "Weld VA-API round-trip probe")?;
    let capabilities = capabilities.context("selected adapter cannot import DMA-BUFs")?;
    let external = capabilities.external_imports()?;
    let xrgb_modifiers = external
        .formats
        .iter()
        .filter_map(|format| (format.fourcc == DRM_FORMAT_XRGB8888).then_some(format.modifier))
        .collect::<Vec<_>>();
    ensure!(
        !xrgb_modifiers.is_empty(),
        "Weld advertises no XRGB8888 modifier"
    );
    println!(
        "adapter={} render-node={} xrgb-modifiers={}",
        adapter_info.name,
        external.render_node.display(),
        xrgb_modifiers.len()
    );

    ensure!(
        xrgb_modifiers.contains(&0),
        "diagnostic source requires a linear XRGB8888 modifier"
    );
    let source_modifiers = vec![0];
    let source = create_xrgb_probe_frame(&external.render_node, WIDTH, HEIGHT, source_modifiers)?;
    print_frame("source-xrgb", &source)?;
    validate_pixels(&sample_probe_frame(
        &device,
        &queue,
        capabilities.clone(),
        &source,
        1,
    )?)?;
    let vpp = VppConverter::open(&external.render_node)?;
    let encoder_input = vpp.convert(&source, VppOutput::Nv12)?;
    print_frame("encoder-nv12", &encoder_input)?;
    let pre_codec = vpp.convert(
        &encoder_input,
        VppOutput::Xrgb8888 {
            modifiers: xrgb_modifiers.clone(),
        },
    )?;
    validate_pixels(&sample_probe_frame(
        &device,
        &queue,
        capabilities.clone(),
        &pre_codec,
        2,
    )?)?;

    let encoder = H264Encoder::open(&external.render_node, WIDTH, HEIGHT, 2_000_000, 60)?;
    let frame_id = MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(1), 1);
    let encoded = encoder.encode_one(frame_id, 0, &encoder_input)?;
    println!("encoded-h264 bytes={}", encoded.payload.len());
    let decoded = decode_h264_frame(&external.render_node, &encoded)?;
    print_frame("decoded-nv12", &decoded)?;

    let normalized = vpp.convert(
        &decoded,
        VppOutput::Xrgb8888 {
            modifiers: xrgb_modifiers,
        },
    )?;
    print_frame("normalized-xrgb", &normalized)?;
    ensure!(
        normalized.fourcc == DRM_FORMAT_XRGB8888,
        "VPP output is not XRGB8888"
    );
    ensure!(
        normalized.planes.len() == 1,
        "VPP XRGB output is not single-plane"
    );
    let modifier = normalized.primary_modifier()?;
    ensure!(
        external.supports(normalized.fourcc, modifier),
        "VPP XRGB output modifier is not advertised by Weld"
    );

    let pixels = sample_probe_frame(&device, &queue, capabilities, &normalized, 3)?;
    validate_pixels(&pixels)?;
    println!("hardware-roundtrip-validated=true diagnostic-readback=true");
    Ok(())
}

fn sample_probe_frame(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    capabilities: weld_core::dmabuf::DmabufCapabilities,
    frame: &VaapiDmabuf,
    sequence: u64,
) -> Result<Vec<u8>> {
    let import_runtime = DmabufContext::for_external_import_probe(device, capabilities);
    let access = import_runtime
        .context()
        .import_external(to_weld_dmabuf(frame)?)?;
    let source_id = ClientSourceId::new(900);
    let lease = import_runtime.context().lease_external(
        ClientBufferId::new(source_id, sequence),
        ClientBufferUseId::new(source_id, sequence),
        ClientBufferMetadata::new(Extent::new(WIDTH, HEIGHT), true),
        access,
        |_| {},
    )?;
    let mut manager = import_runtime
        .context()
        .create_manager(device, queue)?
        .context("Weld did not create its DMA-BUF manager")?;
    let surface = ClientSurfaceId::new(ClientId::new(source_id, sequence), 1);
    let staged = manager.stage(surface, SurfaceLayerId::new(1), lease)?;
    let mut registry = ProbeRegistry::default();
    manager.prepare_render(&HashSet::from([staged.id]), &mut registry)?;
    let promoted = registry
        .image
        .context("Weld did not promote the normalized XRGB frame")?;
    sample_image(device, queue, &promoted)
}

fn print_frame(label: &str, frame: &VaapiDmabuf) -> Result<()> {
    println!(
        "{label} fourcc={} extent={}x{} objects={} planes={} modifier={:#x}",
        fourcc_name(frame.fourcc),
        frame.width,
        frame.height,
        frame.objects.len(),
        frame.planes.len(),
        frame.primary_modifier()?
    );
    Ok(())
}

fn fourcc_name(fourcc: u32) -> String {
    String::from_utf8_lossy(&fourcc.to_le_bytes()).into_owned()
}

fn to_weld_dmabuf(frame: &VaapiDmabuf) -> Result<ExternalDmabuf> {
    let planes = frame
        .planes
        .iter()
        .map(|plane| {
            let object = frame
                .objects
                .get(usize::from(plane.object_index))
                .context("DMA-BUF plane references an absent object")?;
            Ok(ExternalDmabufPlane {
                file_descriptor: object.file_descriptor.try_clone()?,
                offset: plane.offset,
                stride: plane.stride,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ExternalDmabuf {
        extent: Extent::new(frame.width, frame.height),
        format: frame.fourcc,
        modifier: frame.primary_modifier()?,
        flags: 0,
        planes,
    })
}

#[derive(Default)]
struct ProbeRegistry {
    image: Option<PromotionImage>,
}

impl ImportedImageRegistry for ProbeRegistry {
    fn install(&mut self, images: &[PromotionImage]) -> Result<()> {
        let image = images
            .first()
            .context("Weld promoted an empty DMA-BUF batch")?;
        self.image = Some(PromotionImage {
            id: image.id,
            texture: image.texture.clone(),
            view: image.view.clone(),
            format: image.format,
        });
        Ok(())
    }

    fn prune(&mut self, _images: &[weld_core::dmabuf::ImportId]) {}
}

fn sample_image(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    image: &PromotionImage,
) -> Result<Vec<u8>> {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("Weld VA-API round-trip sample shader"),
        source: wgpu::ShaderSource::Wgsl(
            r#"
@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    var uvs = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    var output: VertexOutput;
    output.position = vec4<f32>(positions[index], 0.0, 1.0);
    output.uv = uvs[index];
    return output;
}

@fragment
fn fragment(input: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(source, source_sampler, input.uv);
}
"#
            .into(),
        ),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("Weld VA-API round-trip sample layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("Weld VA-API round-trip sample group"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&image.view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("Weld VA-API round-trip pipeline layout"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("Weld VA-API round-trip pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vertex"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fragment"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8Unorm,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    });
    let output = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Weld VA-API round-trip RGBA output"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let output_view = output.create_view(&Default::default());
    let padded_row = (WIDTH * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Weld VA-API round-trip readback"),
        size: u64::from(padded_row) * u64::from(HEIGHT),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("Weld VA-API round-trip commands"),
    });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Weld VA-API round-trip sample pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &output_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &output,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(HEIGHT),
            },
        },
        output.size(),
    );
    let submission = queue.submit([encoder.finish()]);
    let slice = readback.slice(..);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device.poll(wgpu::PollType::Wait {
        submission_index: Some(submission),
        timeout: None,
    })?;
    receiver.recv().context("readback callback was dropped")??;
    let mapped = slice
        .get_mapped_range()
        .context("readback mapping is unavailable")?;
    let mut pixels = Vec::with_capacity(usize::try_from(WIDTH * HEIGHT * 4)?);
    for row in mapped
        .chunks(usize::try_from(padded_row)?)
        .take(usize::try_from(HEIGHT)?)
    {
        pixels.extend_from_slice(&row[..usize::try_from(WIDTH * 4)?]);
    }
    drop(mapped);
    readback.unmap();
    Ok(pixels)
}

fn validate_pixels(pixels: &[u8]) -> Result<()> {
    const TOLERANCE: i16 = 48;
    let samples = [
        (0, 0),
        (WIDTH - 1, 0),
        (0, HEIGHT - 1),
        (WIDTH - 1, HEIGHT - 1),
        (WIDTH / 2, HEIGHT / 2),
    ];
    for (x, y) in samples {
        let offset = usize::try_from((y * WIDTH + x) * 4)?;
        let actual = pixels
            .get(offset..offset + 4)
            .context("sampled image is truncated")?;
        let horizontal = (x * 127) / WIDTH.saturating_sub(1).max(1);
        let vertical = (y * 127) / HEIGHT.saturating_sub(1).max(1);
        let value = u8::try_from(horizontal + vertical)?;
        let expected = [value, value, value];
        ensure!(
            actual[..3]
                .iter()
                .zip(expected)
                .all(
                    |(actual, expected)| (i16::from(*actual) - i16::from(expected)).abs()
                        <= TOLERANCE
                ),
            "sample at ({x}, {y}) differs from the diagnostic gradient: actual={actual:?} expected={expected:?}"
        );
    }
    Ok(())
}
