//! Project-owned wgpu composition readback and nested presentation.
//!
//! Bevy has already composed client surfaces and shell UI before this module
//! receives a texture. This boundary owns headless readback plus the nested
//! host surface, final blit, presentation, and optional screenshot readback.

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

use crate::dmabuf::{DmabufCapabilities, DmabufSourceCache, request_weld_device};
use crate::host::CompositionFrame;

mod composite;

pub(crate) use composite::CompositionBlitter;

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
    blitter: CompositionBlitter,
    dmabuf_capabilities: Option<DmabufCapabilities>,
    dmabuf_sources: DmabufSourceCache,
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
            apply_limit_buckets: false,
        }))
        .context("no Vulkan adapter can present to the nested window")?;
        let (device, queue, dmabuf_capabilities) =
            request_weld_device(&adapter, "weld nested device")
                .context("failed to create the nested wgpu device")?;
        let dmabuf_sources = DmabufSourceCache::new(&device);

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

        let blitter = CompositionBlitter::new(&device, surface_config.format);

        Ok(Self {
            window,
            instance,
            surface,
            adapter,
            device,
            queue,
            surface_config,
            blitter,
            dmabuf_capabilities,
            dmabuf_sources,
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

    pub(crate) fn dmabuf_capabilities(&self) -> Option<&DmabufCapabilities> {
        self.dmabuf_capabilities.as_ref()
    }

    pub(crate) fn dmabuf_sources(&self) -> DmabufSourceCache {
        self.dmabuf_sources.clone()
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.surface_config.width = size.width;
        self.surface_config.height = size.height;
        self.surface.configure(&self.device, &self.surface_config);
    }

    pub fn render(
        &mut self,
        composition: &wgpu::TextureView,
        capture_path: Option<&Path>,
    ) -> Result<FrameResult> {
        use wgpu::CurrentSurfaceTexture;

        let _present_span = tracing::trace_span!(
            target: crate::PROFILE_TARGET,
            "nested_present_frame"
        )
        .entered();
        let current_surface_texture = {
            let _acquire_span = tracing::trace_span!(
                target: crate::PROFILE_TARGET,
                "acquire_surface_texture"
            )
            .entered();
            self.surface.get_current_texture()
        };
        let (surface_texture, reconfigure_after_present) = match current_surface_texture {
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

        let (submission, capture) = {
            let _submit_span = tracing::trace_span!(
                target: crate::PROFILE_TARGET,
                "encode_submit_present"
            )
            .entered();
            let output_view = surface_texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let composition_bind_group = self.blitter.create_bind_group(
                &self.device,
                "weld Bevy composition bind group",
                composition,
            );
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("weld compositor encoder"),
                });
            self.blitter.encode(
                &mut encoder,
                "weld compositor pass",
                &output_view,
                &composition_bind_group,
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
                self.blitter.encode(
                    &mut encoder,
                    "weld screenshot composition pass",
                    &view,
                    &composition_bind_group,
                );

                let mut capture = encode_capture_readback(
                    &self.device,
                    &mut encoder,
                    &texture,
                    self.surface_config.width,
                    self.surface_config.height,
                    self.surface_config.format,
                );
                capture.retained_texture = Some(texture);
                capture
            });

            let submission = self.queue.submit([encoder.finish()]);
            self.queue.present(surface_texture);
            (submission, capture)
        };

        let capture_result = capture.zip(capture_path).map(|(capture, path)| {
            save_capture_readback(&self.device, capture, submission, path)
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
    retained_texture: Option<wgpu::Texture>,
    buffer: wgpu::Buffer,
    padded_bytes_per_row: u32,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
}

pub(crate) fn capture_owned_frame(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    frame: &CompositionFrame,
    path: &Path,
) -> Result<()> {
    let texture = frame
        .owned_texture()
        .context("capture requires an owned composition texture")?;
    let target = frame.target();
    let extent = target.extent();
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("Weld owned composition readback"),
    });
    let capture = encode_capture_readback(
        device,
        &mut encoder,
        texture,
        extent.width,
        extent.height,
        target.format(),
    );
    let submission = queue.submit([encoder.finish()]);
    save_capture_readback(device, capture, submission, path)
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn read_owned_frame_rgba(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    frame: &CompositionFrame,
) -> Result<Vec<u8>> {
    let texture = frame
        .owned_texture()
        .context("readback requires an owned composition texture")?;
    let target = frame.target();
    let extent = target.extent();
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("Weld test composition readback"),
    });
    let capture = encode_capture_readback(
        device,
        &mut encoder,
        texture,
        extent.width,
        extent.height,
        target.format(),
    );
    let submission = queue.submit([encoder.finish()]);
    read_capture_readback(device, capture, submission)
}

fn encode_capture_readback(
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> CaptureReadback {
    let unpadded_bytes_per_row = width * 4;
    let padded_bytes_per_row =
        unpadded_bytes_per_row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Weld composition readback"),
        size: u64::from(padded_bytes_per_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    CaptureReadback {
        retained_texture: None,
        buffer,
        padded_bytes_per_row,
        width,
        height,
        format,
    }
}

fn save_capture_readback(
    device: &wgpu::Device,
    capture: CaptureReadback,
    submission: wgpu::SubmissionIndex,
    path: &Path,
) -> Result<()> {
    let _capture_span = tracing::trace_span!(
        target: crate::PROFILE_TARGET,
        "capture_readback_encode"
    )
    .entered();
    let width = capture.width;
    let height = capture.height;
    let pixels = read_capture_readback(device, capture, submission)?;
    write_png(path, width, height, &pixels)
}

fn read_capture_readback(
    device: &wgpu::Device,
    capture: CaptureReadback,
    submission: wgpu::SubmissionIndex,
) -> Result<Vec<u8>> {
    let CaptureReadback {
        retained_texture: _retained_texture,
        buffer,
        padded_bytes_per_row,
        width,
        height,
        format,
    } = capture;
    let slice = buffer.slice(..);
    let (sender, receiver) = mpsc::sync_channel(1);
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(CAPTURE_GPU_TIMEOUT),
        })
        .context("composition readback did not complete")?;
    receiver
        .recv_timeout(CAPTURE_GPU_TIMEOUT)
        .context("composition mapping callback did not complete")?
        .context("composition buffer mapping failed")?;
    let mapped = slice
        .get_mapped_range()
        .context("composition mapped range is unavailable")?;
    let pixels = decode_capture_rows(&mapped, width, height, padded_bytes_per_row, format)?;
    drop(mapped);
    buffer.unmap();
    Ok(pixels)
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

pub(crate) fn write_png(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<()> {
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
