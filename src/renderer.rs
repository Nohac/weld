//! Project-owned wgpu composition for the nested validation target.

use std::{
    fs::File,
    io::BufWriter,
    path::Path,
    sync::{Arc, mpsc},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tracing::warn;
use winit::event_loop::OwnedDisplayHandle;
use winit::{dpi::PhysicalSize, window::Window};

use crate::server::{ShmFrame, SurfaceUpdate};

const CAPTURE_GPU_TIMEOUT: Duration = Duration::from_secs(5);

pub struct FrameResult {
    pub presented: bool,
    pub capture: Option<std::result::Result<(), String>>,
}

pub struct NestedRenderer {
    window: Arc<Window>,
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: wgpu::SurfaceConfiguration,
    bind_group_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    client_texture: Option<wgpu::Texture>,
    client_bind_group: Option<wgpu::BindGroup>,
}

impl NestedRenderer {
    pub fn new(
        window: Arc<Window>,
        display: OwnedDisplayHandle,
        size: PhysicalSize<u32>,
    ) -> Result<Self> {
        let mut instance_descriptor =
            wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display));
        instance_descriptor.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(instance_descriptor);
        let surface = instance
            .create_surface(window.clone())
            .context("failed to create the nested wgpu surface")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .context("no Vulkan adapter can present to the nested window")?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("weld nested device"),
            ..Default::default()
        }))
        .context("failed to create the nested wgpu device")?;

        let mut surface_config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("adapter does not support the nested surface")?;
        let capabilities = surface.get_capabilities(&adapter);
        surface_config.format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .context("nested surface has no sRGB format")?;
        surface.configure(&device, &surface_config);

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("weld layer bind group layout"),
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
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("weld compositor pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("weld compositor shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("composite.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("weld compositor pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_config.format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("weld compositor sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Ok(Self {
            window,
            instance,
            surface,
            adapter,
            device,
            queue,
            surface_config,
            bind_group_layout,
            pipeline,
            sampler,
            client_texture: None,
            client_bind_group: None,
        })
    }

    pub fn instance(&self) -> &wgpu::Instance {
        &self.instance
    }

    pub fn adapter(&self) -> &wgpu::Adapter {
        &self.adapter
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.surface_config.width = size.width;
        self.surface_config.height = size.height;
        self.surface.configure(&self.device, &self.surface_config);
    }

    pub fn apply_surface_update(&mut self, update: SurfaceUpdate) {
        match update {
            SurfaceUpdate::Frame(frame) => self.upload_client_frame(frame),
            SurfaceUpdate::Removed => {
                self.client_bind_group = None;
                self.client_texture = None;
            }
        }
    }

    pub fn has_client_frame(&self) -> bool {
        self.client_texture.is_some()
    }

    pub fn render(
        &mut self,
        overlay: &wgpu::TextureView,
        capture_path: Option<&Path>,
    ) -> Result<FrameResult> {
        use wgpu::CurrentSurfaceTexture;

        let (surface_texture, reconfigure_after_present) = match self.surface.get_current_texture()
        {
            CurrentSurfaceTexture::Success(texture) => (texture, false),
            CurrentSurfaceTexture::Suboptimal(texture) => (texture, true),
            CurrentSurfaceTexture::Timeout | CurrentSurfaceTexture::Occluded => {
                return Ok(FrameResult::not_presented());
            }
            CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.surface_config);
                return Ok(FrameResult::not_presented());
            }
            CurrentSurfaceTexture::Lost => {
                self.recreate_surface()?;
                return Ok(FrameResult::not_presented());
            }
            CurrentSurfaceTexture::Validation => {
                bail!("wgpu reported a validation error while acquiring the nested surface")
            }
        };

        let output_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let overlay_bind_group = self.create_bind_group("weld shell overlay bind group", overlay);
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("weld compositor encoder"),
            });
        self.encode_composite_pass(
            &mut encoder,
            "weld compositor pass",
            &output_view,
            &overlay_bind_group,
        );

        let capture = capture_path.map(|_| {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("weld screenshot texture"),
                size: wgpu::Extent3d {
                    width: self.surface_config.width,
                    height: self.surface_config.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.surface_config.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.encode_composite_pass(
                &mut encoder,
                "weld screenshot composition pass",
                &view,
                &overlay_bind_group,
            );

            let unpadded_bytes_per_row = self.surface_config.width * 4;
            let padded_bytes_per_row =
                unpadded_bytes_per_row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("weld screenshot readback"),
                size: u64::from(padded_bytes_per_row) * u64::from(self.surface_config.height),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
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
                        bytes_per_row: Some(padded_bytes_per_row),
                        rows_per_image: Some(self.surface_config.height),
                    },
                },
                wgpu::Extent3d {
                    width: self.surface_config.width,
                    height: self.surface_config.height,
                    depth_or_array_layers: 1,
                },
            );
            CaptureReadback {
                _texture: texture,
                buffer,
                padded_bytes_per_row,
            }
        });

        let submission = self.queue.submit([encoder.finish()]);
        surface_texture.present();

        let capture_result = capture.zip(capture_path).map(|(capture, path)| {
            self.save_capture(capture, submission, path)
                .map_err(|error| error.to_string())
        });

        if reconfigure_after_present {
            self.surface.configure(&self.device, &self.surface_config);
        }
        Ok(FrameResult {
            presented: true,
            capture: capture_result,
        })
    }

    fn encode_composite_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        label: &'static str,
        target: &wgpu::TextureView,
        overlay_bind_group: &wgpu::BindGroup,
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.025,
                        g: 0.032,
                        b: 0.045,
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
        pass.set_pipeline(&self.pipeline);
        if let Some(client_bind_group) = self.client_bind_group.as_ref() {
            pass.set_bind_group(0, client_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        pass.set_bind_group(0, overlay_bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    fn save_capture(
        &self,
        capture: CaptureReadback,
        submission: wgpu::SubmissionIndex,
        path: &Path,
    ) -> Result<()> {
        let slice = capture.buffer.slice(..);
        let (sender, receiver) = mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(CAPTURE_GPU_TIMEOUT),
            })
            .context("GPU screenshot readback did not complete")?;
        receiver
            .recv_timeout(CAPTURE_GPU_TIMEOUT)
            .context("GPU screenshot mapping callback did not complete")?
            .context("GPU screenshot buffer mapping failed")?;

        let mapped = slice.get_mapped_range();
        let pixels = decode_capture_rows(
            &mapped,
            self.surface_config.width,
            self.surface_config.height,
            capture.padded_bytes_per_row,
            self.surface_config.format,
        )?;
        drop(mapped);
        capture.buffer.unmap();
        write_png(
            path,
            self.surface_config.width,
            self.surface_config.height,
            &pixels,
        )
    }

    fn upload_client_frame(&mut self, frame: ShmFrame) {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("weld SHM client texture"),
            size: wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(frame.width * 4),
                rows_per_image: Some(frame.height),
            },
            wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
        );
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.client_bind_group =
            Some(self.create_bind_group("weld client bind group", &texture_view));
        self.client_texture = Some(texture);
    }

    fn create_bind_group(&self, label: &'static str, view: &wgpu::TextureView) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }

    fn recreate_surface(&mut self) -> Result<()> {
        warn!("recreating the lost nested wgpu surface");
        self.surface = self
            .instance
            .create_surface(self.window.clone())
            .context("failed to recreate the nested wgpu surface")?;
        self.surface.configure(&self.device, &self.surface_config);
        Ok(())
    }
}

impl FrameResult {
    const fn not_presented() -> Self {
        Self {
            presented: false,
            capture: None,
        }
    }
}

struct CaptureReadback {
    _texture: wgpu::Texture,
    buffer: wgpu::Buffer,
    padded_bytes_per_row: u32,
}

fn decode_capture_rows(
    mapped: &[u8],
    width: u32,
    height: u32,
    padded_bytes_per_row: u32,
    format: wgpu::TextureFormat,
) -> Result<Vec<u8>> {
    let row_bytes = width as usize * 4;
    let padded_row_bytes = padded_bytes_per_row as usize;
    let required = padded_row_bytes
        .checked_mul(height as usize)
        .context("screenshot dimensions overflow")?;
    if mapped.len() < required || padded_row_bytes < row_bytes {
        bail!("GPU screenshot buffer is shorter than its declared dimensions");
    }

    let mut pixels = Vec::with_capacity(row_bytes * height as usize);
    for row in mapped.chunks_exact(padded_row_bytes).take(height as usize) {
        pixels.extend_from_slice(&row[..row_bytes]);
    }
    match format {
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => {
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
        wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb => {}
        other => bail!("unsupported screenshot surface format {other:?}"),
    }
    Ok(pixels)
}

fn write_png(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<()> {
    let file = File::create(path)
        .with_context(|| format!("failed to create screenshot {}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .context("failed to write PNG header")?;
    writer
        .write_image_data(pixels)
        .context("failed to write PNG pixels")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_capture_padding_and_converts_bgra() {
        let mut mapped = vec![0; 512];
        mapped[..8].copy_from_slice(&[3, 2, 1, 4, 7, 6, 5, 8]);

        let pixels = decode_capture_rows(&mapped, 2, 1, 512, wgpu::TextureFormat::Bgra8UnormSrgb)
            .expect("valid capture rows should decode");

        assert_eq!(pixels, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn preserves_rgba_capture_bytes() {
        let mapped = [1, 2, 3, 4];

        let pixels = decode_capture_rows(&mapped, 1, 1, 4, wgpu::TextureFormat::Rgba8UnormSrgb)
            .expect("valid capture rows should decode");

        assert_eq!(pixels, mapped);
    }
}
