//! DMA-BUF import, encoded-space conversion, and Bevy GPU-image ownership.

use std::{
    collections::{HashMap, HashSet},
    sync::mpsc,
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result, bail};
use ash::vk;
use bevy::{
    app::App,
    asset::{Assets, Handle, RenderAssetUsages},
    image::Image,
    render::{
        RenderApp,
        render_asset::RenderAssets,
        render_resource::{DefaultImageSampler, Texture, TextureView},
        texture::GpuImage,
    },
};
use calloop::channel::Sender as CalloopSender;
use smithay::backend::allocator::Buffer;
use tracing::{debug, error, warn};
use wgpu::util::DeviceExt;

use crate::{
    dmabuf::{DmabufReleaseId, DmabufSourceCache, PendingDmabufFrame},
    surface::{SurfaceId, SurfaceLayerId},
};

const DESTINATION_VIEW_FORMATS: &[wgpu::TextureFormat] = &[wgpu::TextureFormat::Bgra8Unorm];

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TargetKey {
    surface: SurfaceId,
    layer: SurfaceLayerId,
}

struct ImportedTarget {
    handle: Handle<Image>,
    texture: wgpu::Texture,
    size: (u32, u32),
}

struct CompletionWork {
    submission: wgpu::SubmissionIndex,
    release: DmabufReleaseId,
}

enum CompletionCommand {
    Wait(CompletionWork),
    Shutdown,
}

/// Imports client memory, blits it into Weld-owned images, and owns completion.
pub(crate) struct DmabufImporter {
    device: wgpu::Device,
    queue: wgpu::Queue,
    raw_device: ash::Device,
    queue_family: u32,
    bind_group_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    sources: DmabufSourceCache,
    targets: HashMap<TargetKey, ImportedTarget>,
    release_sender: CalloopSender<DmabufReleaseId>,
    completion_sender: mpsc::Sender<CompletionCommand>,
    completion_thread: Option<JoinHandle<()>>,
}

impl DmabufImporter {
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        release_sender: CalloopSender<DmabufReleaseId>,
        sources: DmabufSourceCache,
    ) -> Result<Option<Self>> {
        if !device
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
        {
            return Ok(None);
        }
        // SAFETY: the guard is used to copy the thread-safe ash device handle
        // and immutable queue-family index. Neither native object is destroyed
        // or submitted outside wgpu.
        let (raw_device, queue_family) = unsafe {
            let raw = device
                .as_hal::<wgpu::hal::api::Vulkan>()
                .context("DMA-BUF device is not backed by Vulkan")?;
            (raw.raw_device().clone(), raw.queue_family_index())
        };
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("weld DMA-BUF import bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("weld DMA-BUF import pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("weld DMA-BUF import shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("importer.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("weld DMA-BUF import pipeline"),
            layout: Some(&layout),
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
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let (completion_sender, completion_receiver) = mpsc::channel();
        let completion_device = device.clone();
        let completion_release_sender = release_sender.clone();
        let completion_thread = thread::Builder::new()
            .name("weld-dmabuf-completion".into())
            .spawn(move || {
                completion_loop(
                    completion_device,
                    completion_release_sender,
                    completion_receiver,
                );
            })
            .context("failed to start DMA-BUF completion worker")?;
        Ok(Some(Self {
            device: device.clone(),
            queue: queue.clone(),
            raw_device,
            queue_family,
            bind_group_layout,
            pipeline,
            sources,
            targets: HashMap::new(),
            release_sender,
            completion_sender,
            completion_thread: Some(completion_thread),
        }))
    }

    pub(crate) fn import(
        &mut self,
        app: &mut App,
        surface: SurfaceId,
        layer: SurfaceLayerId,
        frame: PendingDmabufFrame,
        opaque: bool,
    ) -> Result<Handle<Image>> {
        let result = self.import_inner(app, surface, layer, &frame, opaque);
        if result.is_err() {
            let _ = self.release_sender.send(frame.release);
        }
        result
    }

    fn import_inner(
        &mut self,
        app: &mut App,
        surface: SurfaceId,
        layer: SurfaceLayerId,
        frame: &PendingDmabufFrame,
        opaque: bool,
    ) -> Result<Handle<Image>> {
        let size = frame.dmabuf.size();
        let width = u32::try_from(size.w).context("negative DMA-BUF width")?;
        let height = u32::try_from(size.h).context("negative DMA-BUF height")?;
        let source = self
            .sources
            .get(&frame.dmabuf)
            .context("committed DMA-BUF was not imported during protocol creation")?;
        let (target_texture, target_handle) = {
            let target = self.target(app, surface, layer, width, height)?;
            (target.texture.clone(), target.handle.clone())
        };
        let target_view = target_texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("weld DMA-BUF encoded blit target"),
            format: Some(wgpu::TextureFormat::Bgra8Unorm),
            ..Default::default()
        });
        let options = [
            u32::from(frame.dmabuf.y_inverted()),
            u32::from(opaque),
            0,
            0,
        ];
        let option_bytes = options
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        let option_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("weld DMA-BUF import options"),
                contents: &option_bytes,
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("weld DMA-BUF import bind group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&source.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: option_buffer.as_entire_binding(),
                },
            ],
        });
        let acquire = self.external_barrier_command(source.image, true)?;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("weld DMA-BUF import encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("weld DMA-BUF encoded blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        let release = self.external_barrier_command(source.image, false)?;
        // wgpu 30 forbids mixing raw HAL and WebGPU recording in one encoder.
        // One queue submission preserves acquire -> blit -> release ordering
        // while retaining one completion identity and no CPU-side wait. Build
        // every buffer before submitting any of them: the raw buffers refer to
        // the VkImage without retaining it, while the middle tracked buffer
        // keeps the imported wgpu texture alive for the complete submission.
        let commands = [acquire, encoder.finish(), release];
        let submission = self.queue.submit(commands);
        let completion = CompletionCommand::Wait(CompletionWork {
            submission,
            release: frame.release,
        });
        if let Err(mpsc::SendError(CompletionCommand::Wait(work))) =
            self.completion_sender.send(completion)
        {
            self.device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(work.submission),
                    timeout: None,
                })
                .context("DMA-BUF fallback completion wait failed")?;
            bail!("DMA-BUF completion worker stopped");
        }
        debug!(
            ?surface,
            ?layer,
            width,
            height,
            "submitted DMA-BUF GPU blit"
        );
        Ok(target_handle)
    }

    fn target(
        &mut self,
        app: &mut App,
        surface: SurfaceId,
        layer: SurfaceLayerId,
        width: u32,
        height: u32,
    ) -> Result<&ImportedTarget> {
        let key = TargetKey { surface, layer };
        let reusable = self
            .targets
            .get(&key)
            .is_some_and(|target| target.size == (width, height));
        if !reusable {
            self.targets.remove(&key);
            let extent = wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            };
            let descriptor = wgpu::TextureDescriptor {
                label: Some("weld client surface image"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8UnormSrgb,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: DESTINATION_VIEW_FORMATS,
            };
            let texture = self.device.create_texture(&descriptor);
            let image = Image::new_uninit(
                extent,
                wgpu::TextureDimension::D2,
                wgpu::TextureFormat::Bgra8UnormSrgb,
                RenderAssetUsages::MAIN_WORLD,
            );
            let handle = app
                .world_mut()
                .get_resource_mut::<Assets<Image>>()
                .context("Bevy image assets are unavailable")?
                .add(image);
            let render_app = app
                .get_sub_app_mut(RenderApp)
                .context("Bevy RenderApp is unavailable")?;
            let sampler = render_app.world().resource::<DefaultImageSampler>().clone();
            let gpu_texture = Texture::from(texture.clone());
            let gpu_view =
                TextureView::from(texture.create_view(&wgpu::TextureViewDescriptor::default()));
            render_app
                .world_mut()
                .resource_mut::<RenderAssets<GpuImage>>()
                .insert(
                    handle.id(),
                    GpuImage {
                        texture: gpu_texture,
                        texture_view: gpu_view,
                        sampler: (*sampler).clone(),
                        texture_descriptor: descriptor,
                        texture_view_descriptor: None,
                        had_data: false,
                    },
                );
            self.targets.insert(
                key,
                ImportedTarget {
                    handle,
                    texture,
                    size: (width, height),
                },
            );
        }
        self.targets
            .get(&key)
            .context("DMA-BUF target insertion failed")
    }

    fn external_barrier_command(
        &self,
        image: vk::Image,
        acquire: bool,
    ) -> Result<wgpu::CommandBuffer> {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some(if acquire {
                    "weld DMA-BUF foreign acquire"
                } else {
                    "weld DMA-BUF foreign release"
                }),
            });
        let queue_family = self.queue_family;
        let raw_device = &self.raw_device;
        let recorded =
            // SAFETY: the callback records one Vulkan barrier into wgpu's
            // active command buffer without ending or submitting it. The image
            // belongs to this device and is kept alive through queue submit.
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
                    let barrier = vk::ImageMemoryBarrier::default()
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
                        });
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
                        &[barrier],
                    );
                    Some(())
                })
            };
        recorded.context("wgpu command encoder is not backed by Vulkan")?;
        Ok(encoder.finish())
    }

    pub(crate) fn retain_surface_layers(
        &mut self,
        surface: SurfaceId,
        retained: &HashSet<SurfaceLayerId>,
    ) {
        self.targets
            .retain(|key, _| key.surface != surface || retained.contains(&key.layer));
    }

    pub(crate) fn remove_surface(&mut self, surface: SurfaceId) {
        self.targets.retain(|key, _| key.surface != surface);
    }

    pub(crate) fn remove_layer(&mut self, surface: SurfaceId, layer: SurfaceLayerId) {
        self.targets.remove(&TargetKey { surface, layer });
    }
}

impl Drop for DmabufImporter {
    fn drop(&mut self) {
        let _ = self.completion_sender.send(CompletionCommand::Shutdown);
        if let Some(thread) = self.completion_thread.take()
            && thread.join().is_err()
        {
            error!("DMA-BUF completion worker panicked during shutdown");
        }
    }
}

fn completion_loop(
    device: wgpu::Device,
    release_sender: CalloopSender<DmabufReleaseId>,
    receiver: mpsc::Receiver<CompletionCommand>,
) {
    while let Ok(command) = receiver.recv() {
        let CompletionCommand::Wait(work) = command else {
            break;
        };
        let result = device.poll(wgpu::PollType::Wait {
            submission_index: Some(work.submission),
            timeout: None,
        });
        if let Err(error) = result {
            warn!(%error, ?work.release, "DMA-BUF GPU completion wait failed; releasing client buffer during recovery");
        }
        if release_sender.send(work.release).is_err() {
            break;
        }
    }
}
