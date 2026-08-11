//! Direct DMA-BUF sampling and client-buffer lifetime ownership.

use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
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
use tracing::{debug, error, warn};

use crate::{
    dmabuf::{DmabufReleaseId, DmabufSourceCache, ImportedDmabufSource, PendingDmabufFrame},
    surface::{SurfaceId, SurfaceImageEncoding, SurfaceLayerId, SurfaceRenderImage},
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SurfaceLayerKey {
    surface: SurfaceId,
    layer: SurfaceLayerId,
}

struct StagedImage {
    handle: Handle<Image>,
    source: Rc<ImportedDmabufSource>,
    release: DmabufReleaseId,
}

struct DisplayedImage {
    handle: Handle<Image>,
    source: Rc<ImportedDmabufSource>,
    release: DmabufReleaseId,
}

#[derive(Default)]
struct SurfaceLayerImages {
    staged: Option<StagedImage>,
    displayed: Option<DisplayedImage>,
}

#[derive(Default)]
struct AcquiredSources(HashMap<vk::Image, usize>);

struct SourceRetirementPlan {
    counts: HashMap<vk::Image, usize>,
    releases: Vec<vk::Image>,
}

impl AcquiredSources {
    fn contains(&self, image: vk::Image) -> bool {
        self.0.contains_key(&image)
    }

    fn retain(&mut self, image: vk::Image) {
        *self.0.entry(image).or_default() += 1;
    }

    fn plan_retirements(
        &self,
        images: impl IntoIterator<Item = vk::Image>,
    ) -> Result<SourceRetirementPlan> {
        let mut counts = HashMap::<_, usize>::new();
        for image in images {
            *counts.entry(image).or_default() += 1;
        }
        let mut releases = Vec::new();
        for (&image, &retiring) in &counts {
            let acquired = self.0.get(&image).copied().unwrap_or_default();
            if retiring > acquired {
                bail!(
                    "DMA-BUF source retirement underflow: retiring {retiring} of {acquired} uses"
                );
            }
            if retiring == acquired {
                releases.push(image);
            }
        }
        Ok(SourceRetirementPlan { counts, releases })
    }

    fn commit_retirements(&mut self, plan: &SourceRetirementPlan) {
        for (&image, &retiring) in &plan.counts {
            let Some(acquired) = self.0.get_mut(&image) else {
                continue;
            };
            *acquired -= retiring;
            if *acquired == 0 {
                self.0.remove(&image);
            }
        }
    }

    fn images(&self) -> impl Iterator<Item = vk::Image> + '_ {
        self.0.keys().copied()
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

struct CompletionWork {
    submission: wgpu::SubmissionIndex,
    releases: Vec<DmabufReleaseId>,
    _textures: Vec<wgpu::Texture>,
}

enum CompletionCommand {
    Wait(CompletionWork),
    Shutdown,
}

/// Owns direct client-image promotion, retirement, and GPU completion.
pub(crate) struct DmabufImporter {
    device: wgpu::Device,
    queue: wgpu::Queue,
    raw_device: ash::Device,
    queue_family: u32,
    sources: DmabufSourceCache,
    layers: HashMap<SurfaceLayerKey, SurfaceLayerImages>,
    superseded: Vec<StagedImage>,
    retiring: Vec<DisplayedImage>,
    acquired_sources: AcquiredSources,
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
            sources,
            layers: HashMap::new(),
            superseded: Vec::new(),
            retiring: Vec::new(),
            acquired_sources: AcquiredSources::default(),
            release_sender,
            completion_sender,
            completion_thread: Some(completion_thread),
        }))
    }

    /// Stage a fresh client-buffer identity without acquiring it for rendering yet.
    pub(crate) fn import(
        &mut self,
        app: &mut App,
        surface: SurfaceId,
        layer: SurfaceLayerId,
        frame: PendingDmabufFrame,
        opaque: bool,
    ) -> Result<SurfaceRenderImage> {
        let result = self.stage(app, surface, layer, &frame, opaque);
        if result.is_err() {
            let _ = self.release_sender.send(frame.release);
        }
        result
    }

    fn stage(
        &mut self,
        app: &mut App,
        surface: SurfaceId,
        layer: SurfaceLayerId,
        frame: &PendingDmabufFrame,
        opaque: bool,
    ) -> Result<SurfaceRenderImage> {
        let source = self
            .sources
            .get(&frame.dmabuf)
            .context("committed DMA-BUF was not imported during protocol creation")?;
        let extent = source.texture.size();
        let image = Image::new_uninit(
            extent,
            wgpu::TextureDimension::D2,
            source.format,
            RenderAssetUsages::MAIN_WORLD,
        );
        let handle = app
            .world_mut()
            .get_resource_mut::<Assets<Image>>()
            .context("Bevy image assets are unavailable")?
            .add(image);
        let key = SurfaceLayerKey { surface, layer };
        let layer_images = self.layers.entry(key).or_default();
        if let Some(staged) = layer_images.staged.replace(StagedImage {
            handle: handle.clone(),
            source,
            release: frame.release,
        }) {
            // Its owned placeholder may still be referenced by a queued ECS
            // snapshot. Keep it until the next main-world advance has drained
            // those events, then release it without a GPU ownership transfer.
            self.superseded.push(staged);
        }
        Ok(SurfaceRenderImage {
            image: handle,
            encoding: if opaque {
                SurfaceImageEncoding::EncodedOpaque
            } else {
                SurfaceImageEncoding::EncodedPremultiplied
            },
            y_inverted: frame.dmabuf.y_inverted(),
        })
    }

    /// Promote the newest referenced buffers before Bevy records its composition.
    pub(crate) fn prepare_render(&mut self, app: &mut App) -> Result<()> {
        let superseded = std::mem::take(&mut self.superseded);
        self.release_never_acquired(app, superseded);

        let sampler = {
            let render_app = app
                .get_sub_app(RenderApp)
                .context("Bevy RenderApp is unavailable")?;
            render_app
                .world()
                .get_resource::<RenderAssets<GpuImage>>()
                .context("Bevy GPU image assets are unavailable")?;
            render_app
                .world()
                .get_resource::<DefaultImageSampler>()
                .context("Bevy default image sampler is unavailable")?
                .clone()
        };
        let referenced_handles = {
            let images = app
                .world()
                .get_resource::<Assets<Image>>()
                .context("Bevy image assets are unavailable")?;
            self.layers
                .values()
                .filter_map(|layer| layer.staged.as_ref())
                .filter(|staged| images.contains(&staged.handle))
                .map(|staged| staged.handle.id())
                .collect::<HashSet<_>>()
        };

        let staged = self
            .layers
            .iter_mut()
            .filter_map(|(key, images)| images.staged.take().map(|image| (*key, image)))
            .collect::<Vec<_>>();
        if staged.is_empty() {
            return Ok(());
        }

        let referenced = staged
            .into_iter()
            .partition::<Vec<_>, _>(|(_, staged)| referenced_handles.contains(&staged.handle.id()));
        let (referenced, discarded) = referenced;
        self.release_never_acquired(app, discarded.into_iter().map(|(_, image)| image).collect());
        if referenced.is_empty() {
            return Ok(());
        }

        let mut promotions = Vec::with_capacity(referenced.len());
        let mut commands = Vec::with_capacity(referenced.len());
        let mut failed = Vec::new();
        let mut planned_acquires = HashSet::new();
        {
            let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
                self.release_never_acquired(
                    app,
                    referenced.into_iter().map(|(_, image)| image).collect(),
                );
                bail!("Bevy RenderApp disappeared while promoting DMA-BUFs");
            };
            let Some(mut gpu_images) = render_app
                .world_mut()
                .get_resource_mut::<RenderAssets<GpuImage>>()
            else {
                self.release_never_acquired(
                    app,
                    referenced.into_iter().map(|(_, image)| image).collect(),
                );
                bail!("Bevy GPU image assets disappeared while promoting DMA-BUFs");
            };
            for (key, staged) in referenced {
                let needs_acquire = !self.acquired_sources.contains(staged.source.image)
                    && !planned_acquires.contains(&staged.source.image);
                if needs_acquire {
                    match self.external_barrier_command(staged.source.image, true) {
                        Ok(command) => {
                            commands.push(command);
                            planned_acquires.insert(staged.source.image);
                        }
                        Err(error) => {
                            failed.push(staged);
                            warn!(%error, ?key.surface, ?key.layer, "could not acquire a staged DMA-BUF");
                            continue;
                        }
                    }
                }
                let descriptor = imported_texture_descriptor(&staged.source);
                gpu_images.insert(
                    staged.handle.id(),
                    GpuImage {
                        texture: Texture::from(staged.source.texture.clone()),
                        texture_view: TextureView::from(staged.source.view.clone()),
                        sampler: (*sampler).clone(),
                        texture_descriptor: descriptor,
                        texture_view_descriptor: None,
                        had_data: false,
                    },
                );
                promotions.push((key, staged));
            }
            if !failed.is_empty() {
                for (_, staged) in promotions.drain(..) {
                    gpu_images.remove(staged.handle.id());
                    failed.push(staged);
                }
            }
        }
        let acquisition_failed = !failed.is_empty();
        self.release_never_acquired(app, failed);
        if acquisition_failed {
            bail!("could not acquire the complete staged DMA-BUF batch");
        }
        if promotions.is_empty() {
            return Ok(());
        }

        self.queue.submit(commands);
        for (key, staged) in promotions {
            self.acquired_sources.retain(staged.source.image);
            let images = self.layers.entry(key).or_default();
            if let Some(displayed) = images.displayed.replace(DisplayedImage {
                handle: staged.handle,
                source: staged.source,
                release: staged.release,
            }) {
                self.retiring.push(displayed);
            }
            debug!(?key.surface, ?key.layer, "promoted direct DMA-BUF image");
        }
        Ok(())
    }

    /// Retire replaced buffers after Bevy has submitted every possible old-image read.
    pub(crate) fn finish_render(&mut self, app: &mut App) -> Result<()> {
        if self.retiring.is_empty() {
            return Ok(());
        }
        let retirement = self
            .acquired_sources
            .plan_retirements(self.retiring.iter().map(|image| image.source.image))?;
        let mut commands = Vec::with_capacity(retirement.releases.len());
        for image in &retirement.releases {
            commands.push(self.external_barrier_command(*image, false)?);
        }
        {
            let render_app = app
                .get_sub_app_mut(RenderApp)
                .context("Bevy RenderApp is unavailable")?;
            let mut gpu_images = render_app
                .world_mut()
                .get_resource_mut::<RenderAssets<GpuImage>>()
                .context("Bevy GPU image assets are unavailable")?;
            for image in &self.retiring {
                gpu_images.remove(image.handle.id());
            }
        }
        self.acquired_sources.commit_retirements(&retirement);
        let retiring = std::mem::take(&mut self.retiring);
        remove_placeholders(app, retiring.iter().map(|image| &image.handle));

        let submission = self.queue.submit(commands);
        let work = CompletionWork {
            submission,
            releases: retiring.iter().map(|image| image.release).collect(),
            _textures: retiring
                .iter()
                .map(|image| image.source.texture.clone())
                .collect(),
        };
        self.queue_completion(work);
        Ok(())
    }

    fn release_never_acquired(&self, app: &mut App, images: Vec<StagedImage>) {
        remove_placeholders(app, images.iter().map(|image| &image.handle));
        for image in images {
            let _ = self.release_sender.send(image.release);
        }
    }

    fn queue_completion(&self, work: CompletionWork) {
        if let Err(mpsc::SendError(CompletionCommand::Wait(work))) =
            self.completion_sender.send(CompletionCommand::Wait(work))
        {
            let result = self.device.poll(wgpu::PollType::Wait {
                submission_index: Some(work.submission),
                timeout: None,
            });
            if let Err(error) = result {
                warn!(%error, "DMA-BUF fallback completion wait failed");
            }
            for release in work.releases {
                let _ = self.release_sender.send(release);
            }
        }
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
        let removed = self
            .layers
            .keys()
            .copied()
            .filter(|key| key.surface == surface && !retained.contains(&key.layer))
            .collect::<Vec<_>>();
        for key in removed {
            self.remove_key(key);
        }
    }

    pub(crate) fn remove_surface(&mut self, surface: SurfaceId) {
        let removed = self
            .layers
            .keys()
            .copied()
            .filter(|key| key.surface == surface)
            .collect::<Vec<_>>();
        for key in removed {
            self.remove_key(key);
        }
    }

    pub(crate) fn remove_layer(&mut self, surface: SurfaceId, layer: SurfaceLayerId) {
        self.remove_key(SurfaceLayerKey { surface, layer });
    }

    fn remove_key(&mut self, key: SurfaceLayerKey) {
        let Some(images) = self.layers.remove(&key) else {
            return;
        };
        if let Some(staged) = images.staged {
            self.superseded.push(staged);
        }
        if let Some(displayed) = images.displayed {
            self.retiring.push(displayed);
        }
    }

    fn submit_shutdown_releases(&mut self) {
        let mut staged = std::mem::take(&mut self.superseded);
        let layers = std::mem::take(&mut self.layers);
        let mut displayed = std::mem::take(&mut self.retiring);
        for images in layers.into_values() {
            staged.extend(images.staged);
            displayed.extend(images.displayed);
        }
        for image in staged {
            let _ = self.release_sender.send(image.release);
        }
        if let Err(error) = self
            .acquired_sources
            .plan_retirements(displayed.iter().map(|image| image.source.image))
        {
            warn!(%error, "could not validate acquired DMA-BUFs during shutdown");
            return;
        }
        let acquired_images = self.acquired_sources.images().collect::<Vec<_>>();
        if acquired_images.is_empty() {
            return;
        }
        let commands = acquired_images
            .iter()
            .filter_map(|image| {
                self.external_barrier_command(*image, false)
                    .map_err(|error| {
                        warn!(%error, "could not release an acquired DMA-BUF during shutdown");
                    })
                    .ok()
            })
            .collect::<Vec<_>>();
        if commands.len() != acquired_images.len() {
            return;
        }
        let submission = self.queue.submit(commands);
        self.acquired_sources.clear();
        self.queue_completion(CompletionWork {
            submission,
            releases: displayed.iter().map(|image| image.release).collect(),
            _textures: displayed
                .drain(..)
                .map(|image| image.source.texture.clone())
                .collect(),
        });
    }
}

impl Drop for DmabufImporter {
    fn drop(&mut self) {
        self.submit_shutdown_releases();
        let _ = self.completion_sender.send(CompletionCommand::Shutdown);
        if let Some(thread) = self.completion_thread.take()
            && thread.join().is_err()
        {
            error!("DMA-BUF completion worker panicked during shutdown");
        }
    }
}

fn imported_texture_descriptor(source: &ImportedDmabufSource) -> wgpu::TextureDescriptor<'static> {
    wgpu::TextureDescriptor {
        label: Some("weld direct client DMA-BUF"),
        size: source.texture.size(),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: source.format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    }
}

fn remove_placeholders<'a>(app: &mut App, handles: impl IntoIterator<Item = &'a Handle<Image>>) {
    let Some(mut images) = app.world_mut().get_resource_mut::<Assets<Image>>() else {
        return;
    };
    for handle in handles {
        images.remove(handle.id());
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
            warn!(%error, releases = ?work.releases, "DMA-BUF GPU completion wait failed; releasing client buffers during recovery");
        }
        for release in work.releases {
            if release_sender.send(release).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ash::vk::Handle;

    use super::AcquiredSources;

    fn image(raw: u64) -> ash::vk::Image {
        ash::vk::Image::from_raw(raw)
    }

    #[test]
    fn shared_source_stays_acquired_until_its_final_use_retires() {
        let source = image(1);
        let mut acquired = AcquiredSources::default();
        acquired.retain(source);
        acquired.retain(source);

        let first_retirement = acquired.plan_retirements([source]).unwrap();
        assert!(first_retirement.releases.is_empty());
        acquired.commit_retirements(&first_retirement);
        assert!(acquired.contains(source));

        let final_retirement = acquired.plan_retirements([source]).unwrap();
        assert_eq!(final_retirement.releases, [source]);
        acquired.commit_retirements(&final_retirement);
        assert!(!acquired.contains(source));
    }

    #[test]
    fn one_retirement_batch_releases_a_shared_source_once() {
        let source = image(1);
        let mut acquired = AcquiredSources::default();
        acquired.retain(source);
        acquired.retain(source);

        let retirement = acquired.plan_retirements([source, source]).unwrap();
        assert_eq!(retirement.releases, [source]);
        acquired.commit_retirements(&retirement);
        assert!(!acquired.contains(source));
    }

    #[test]
    fn retirement_underflow_is_rejected_without_mutating_the_ledger() {
        let source = image(1);
        let mut acquired = AcquiredSources::default();
        acquired.retain(source);

        assert!(acquired.plan_retirements([source, source]).is_err());
        assert!(acquired.contains(source));
    }
}
