//! GPU composition into reusable DMA-BUF targets for encoded presentation.

use std::{
    collections::{HashMap, HashSet},
    fs::OpenOptions,
    os::unix::fs::MetadataExt,
    path::Path,
    rc::Rc,
};

use anyhow::{Context, Result, ensure};
use ash::vk;
use smithay::backend::allocator::{
    Allocator, Fourcc, Modifier,
    dmabuf::{AsDmabuf, Dmabuf},
    gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
};
use weld_client::{ClientBufferId, ComposedBuffer, Extent};

use super::sync::sampling_barrier_command;
use super::target::{
    ForeignImageBarrier, ImportedRenderTarget, adapter_matches_device,
    foreign_image_barrier_command, import_render_target, modifiers_for_usage,
};
use super::{
    DirectClientBufferAccess, DmabufSourceCache, ExternalDmabuf, export_client_dmabuf,
    request_weld_device,
};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

struct Target {
    imported: ImportedRenderTarget,
    dmabuf: Dmabuf,
}

/// Retaining this result reserves its render target until the encoder finishes.
pub struct ComposedImage {
    target: Rc<Target>,
}

impl ComposedImage {
    pub fn export(&self) -> Result<ExternalDmabuf> {
        super::export_dmabuf(&self.target.dmabuf)
    }
}

struct InputTexture {
    unused_frames: u8,
    binding: wgpu::BindGroup,
    dmabuf: Option<Dmabuf>,
    image: Option<vk::Image>,
    inverted: bool,
}

struct GeometrySlot {
    buffer: wgpu::Buffer,
    binding: wgpu::BindGroup,
}

/// Created lazily for sources requesting a composed view. Imports, target storage,
/// and draw uniforms are reused while their buffer identities remain live.
pub struct SourceCompositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    raw_device: ash::Device,
    queue_family: u32,
    sources: DmabufSourceCache,
    allocator: GbmAllocator<std::fs::File>,
    modifiers: Vec<Modifier>,
    pipeline: wgpu::RenderPipeline,
    texture_layout: wgpu::BindGroupLayout,
    geometry_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    textures: HashMap<ClientBufferId, InputTexture>,
    geometry: Vec<GeometrySlot>,
    targets: Vec<Rc<Target>>,
}

impl SourceCompositor {
    pub fn new(render_node: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(render_node)
            .context("opening composition render node")?;
        let device_id = file.metadata()?.rdev();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN))
            .into_iter()
            .find(|adapter| adapter_matches_device(adapter, device_id))
            .context("no Vulkan adapter for composition render node")?;
        // SAFETY: the guard only queries immutable capabilities of this live adapter.
        let modifiers = unsafe {
            let raw = adapter
                .as_hal::<wgpu::hal::api::Vulkan>()
                .context("composition requires Vulkan")?;
            modifiers_for_usage(
                raw.shared_instance().raw_instance(),
                raw.raw_physical_device(),
                vk::Format::B8G8R8A8_UNORM,
                vk::FormatFeatureFlags::COLOR_ATTACHMENT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT,
            )?
        }
        .into_iter()
        .map(Modifier::from)
        .collect::<Vec<_>>();
        ensure!(
            !modifiers.is_empty(),
            "no exportable composition target modifier"
        );
        let (device, queue, _) = request_weld_device(&adapter, "Weld source composition")?;
        // SAFETY: copied handles stay valid while this object owns the device.
        let (raw_device, queue_family) = unsafe {
            let raw = device
                .as_hal::<wgpu::hal::api::Vulkan>()
                .context("composition requires Vulkan")?;
            (raw.raw_device().clone(), raw.queue_family_index())
        };
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("composition pixels"),
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
        let geometry_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("composition geometry"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("source composition"),
            bind_group_layouts: &[Some(&texture_layout), Some(&geometry_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("source composition"),
            source: wgpu::ShaderSource::Wgsl(include_str!("composition.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("source composition"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("source composition"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Ok(Self {
            sources: DmabufSourceCache::new(&device),
            device,
            queue,
            raw_device,
            queue_family,
            allocator: GbmAllocator::new(GbmDevice::new(file)?, GbmBufferFlags::RENDERING),
            modifiers,
            pipeline,
            texture_layout,
            geometry_layout,
            sampler,
            textures: HashMap::new(),
            geometry: Vec::new(),
            targets: Vec::new(),
        })
    }

    pub fn compose(&mut self, frame: &ComposedBuffer) -> Result<ComposedImage> {
        let extent = frame.extent;
        let limit = self.device.limits().max_texture_dimension_2d;
        ensure!(
            extent.width > 0
                && extent.height > 0
                && extent.width <= limit
                && extent.height <= limit,
            "composition extent exceeds GPU capabilities"
        );
        ensure!(
            frame.logical_size.width > 0.0
                && frame.logical_size.height > 0.0
                && frame.logical_size.width.is_finite()
                && frame.logical_size.height.is_finite(),
            "invalid composition logical size"
        );
        let live = frame
            .layers
            .iter()
            .map(|layer| layer.buffer.buffer())
            .collect::<HashSet<_>>();
        self.textures.retain(|id, texture| {
            texture.unused_frames = if live.contains(id) {
                0
            } else {
                texture.unused_frames.saturating_add(1)
            };
            // Keep a short rotation of imports for double/triple-buffered producers.
            let retained = texture.unused_frames <= 3;
            if !retained && let Some(dmabuf) = &texture.dmabuf {
                self.sources.remove(dmabuf);
            }
            retained
        });
        for (index, layer) in frame.layers.iter().enumerate() {
            self.prepare_texture(&layer.buffer)?;
            while self.geometry.len() <= index {
                let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("composition layer geometry"),
                    size: 48,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let binding = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("composition layer geometry"),
                    layout: &self.geometry_layout,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: buffer.as_entire_binding(),
                    }],
                });
                self.geometry.push(GeometrySlot { buffer, binding });
            }
            let texture = self
                .textures
                .get(&layer.buffer.buffer())
                .context("composition import disappeared")?;
            let p = layer.placement;
            let size = layer.buffer.metadata().extent;
            let values = [
                p.position.x / frame.logical_size.width,
                p.position.y / frame.logical_size.height,
                p.view.logical_width / frame.logical_size.width,
                p.view.logical_height / frame.logical_size.height,
                p.view.source_x / size.width as f32,
                p.view.source_y / size.height as f32,
                p.view.source_width / size.width as f32,
                p.view.source_height / size.height as f32,
                if layer.buffer.metadata().opaque {
                    1.0
                } else {
                    0.0
                },
                if texture.inverted { 1.0 } else { 0.0 },
                0.0,
                0.0,
            ];
            ensure!(
                values.iter().all(|value| value.is_finite()),
                "invalid composition layer geometry"
            );
            let mut bytes = [0; 48];
            for (value, destination) in values.iter().zip(bytes.chunks_exact_mut(4)) {
                destination.copy_from_slice(&value.to_ne_bytes());
            }
            self.queue
                .write_buffer(&self.geometry[index].buffer, 0, &bytes);
        }
        let target = self.target(extent)?;
        let images = self
            .textures
            .iter()
            .filter(|(id, _)| live.contains(id))
            .filter_map(|(_, input)| input.image)
            .collect::<Vec<_>>();
        let acquire = sampling_barrier_command(
            &self.device,
            &self.raw_device,
            self.queue_family,
            &images,
            true,
        )?;
        let release = sampling_barrier_command(
            &self.device,
            &self.raw_device,
            self.queue_family,
            &images,
            false,
        )?;
        // SAFETY: target storage belongs to this device and is exclusively reserved
        // through GPU completion and the subsequent encoder read.
        let (target_acquire, target_release) = unsafe {
            (
                foreign_image_barrier_command(
                    &self.device,
                    &self.raw_device,
                    self.queue_family,
                    target.imported.image,
                    target.imported.used.get(),
                    ForeignImageBarrier::Acquire,
                )?,
                foreign_image_barrier_command(
                    &self.device,
                    &self.raw_device,
                    self.queue_family,
                    target.imported.image,
                    true,
                    ForeignImageBarrier::Release,
                )?,
            )
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("source composition"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("source composition"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.imported.view,
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
            pass.set_pipeline(&self.pipeline);
            for (layer, geometry) in frame.layers.iter().zip(&self.geometry) {
                let texture = self
                    .textures
                    .get(&layer.buffer.buffer())
                    .context("composition import disappeared")?;
                pass.set_bind_group(0, &texture.binding, &[]);
                pass.set_bind_group(1, &geometry.binding, &[]);
                pass.draw(0..6, 0..1);
            }
        }
        let submission = self.queue.submit(
            acquire
                .into_iter()
                .chain([target_acquire, encoder.finish(), target_release])
                .chain(release),
        );
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: None,
            })
            .context("waiting for source composition")?;
        target.imported.used.set(true);
        Ok(ComposedImage { target })
    }

    fn target(&mut self, extent: Extent) -> Result<Rc<Target>> {
        self.targets.retain(|target| {
            Rc::strong_count(target) > 1
                || target.imported.texture.width() == extent.width
                    && target.imported.texture.height() == extent.height
        });
        if let Some(target) = self
            .targets
            .iter()
            .find(|target| Rc::strong_count(target) == 1)
        {
            return Ok(target.clone());
        }
        ensure!(
            self.targets.len() < 3,
            "composition render targets are still leased"
        );
        let buffer = self.allocator.create_buffer(
            extent.width,
            extent.height,
            Fourcc::Argb8888,
            &self.modifiers,
        )?;
        let dmabuf = buffer.export()?;
        ensure!(
            dmabuf.num_planes() == 1,
            "composition requires a single-plane render target"
        );
        let imported = import_render_target(&self.device, &dmabuf, FORMAT)?;
        let target = Rc::new(Target { imported, dmabuf });
        self.targets.push(target.clone());
        Ok(target)
    }

    fn prepare_texture(&mut self, lease: &weld_client::ClientBufferLease) -> Result<()> {
        if self.textures.contains_key(&lease.buffer()) {
            return Ok(());
        }
        let (texture, dmabuf, image, inverted) = match lease
            .access::<DirectClientBufferAccess>()
            .context("composition requires native buffer access")?
        {
            DirectClientBufferAccess::Dmabuf(_) => {
                let dmabuf = export_client_dmabuf(lease)?.into_smithay()?;
                let source = self.sources.import(&dmabuf)?;
                (
                    source.texture.clone(),
                    Some(dmabuf.clone()),
                    Some(source.image),
                    dmabuf.y_inverted(),
                )
            }
            DirectClientBufferAccess::Shm(shm) => {
                let extent = lease.metadata().extent;
                let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("composed SHM input"),
                    size: wgpu::Extent3d {
                        width: extent.width,
                        height: extent.height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: FORMAT,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                upload(&self.queue, &texture, &shm.bgra_pixels, extent)?;
                (texture, None, None, false)
            }
        };
        let view = texture.create_view(&Default::default());
        let binding = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("composition input"),
            layout: &self.texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.textures.insert(
            lease.buffer(),
            InputTexture {
                unused_frames: 0,
                binding,
                dmabuf,
                image,
                inverted,
            },
        );
        Ok(())
    }
}

fn upload(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    pixels: &[u8],
    extent: Extent,
) -> Result<()> {
    ensure!(
        u64::from(extent.width) * u64::from(extent.height) * 4 == pixels.len() as u64,
        "invalid composition SHM size"
    );
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(extent.width * 4),
            rows_per_image: Some(extent.height),
        },
        texture.size(),
    );
    Ok(())
}

#[cfg(test)]
mod tests;
