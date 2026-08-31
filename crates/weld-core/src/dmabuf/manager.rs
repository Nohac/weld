//! Direct DMA-BUF sampling and client-buffer lifetime ownership.

use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::mpsc,
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result, bail};
use ash::vk;
use calloop::channel::{Channel as CalloopChannel, Sender as CalloopSender};
use smithay::backend::allocator::Buffer;
use tracing::{debug, error, warn};

use super::{
    DirectClientBufferAccess, DmabufAccess, DmabufCapabilities, DmabufEvent, DmabufSourceCache,
    ExternalDmabuf, ImportId, ImportedDmabufSource, PendingWaylandDmabufUse,
};
use crate::surface::{SurfaceId, SurfaceLayerId};
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, ClientSourceId,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SurfaceLayerKey {
    surface: SurfaceId,
    layer: SurfaceLayerId,
}

struct StagedImage {
    id: ImportId,
    source: Rc<ImportedDmabufSource>,
    use_id: ClientBufferUseId,
    lease: ClientBufferLease,
}

struct DisplayedImage {
    id: ImportId,
    source: Rc<ImportedDmabufSource>,
    use_id: ClientBufferUseId,
    lease: ClientBufferLease,
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
    uses: Vec<ClientBufferUseId>,
    _textures: Vec<wgpu::Texture>,
}

#[derive(Default)]
struct PendingGpuLeases(HashMap<ClientBufferUseId, ClientBufferLease>);

impl PendingGpuLeases {
    fn retain(&mut self, use_id: ClientBufferUseId, lease: ClientBufferLease) {
        self.0.insert(use_id, lease);
    }

    fn complete(&mut self, use_id: ClientBufferUseId) {
        self.0.remove(&use_id);
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

enum CompletionCommand {
    Wait(CompletionWork),
    Shutdown,
}

/// Metadata needed to allocate the application-side placeholder image.
pub struct StagedImport {
    pub id: ImportId,
    pub extent: wgpu::Extent3d,
    pub format: wgpu::TextureFormat,
    pub y_inverted: bool,
}

/// One native image in an all-or-nothing promotion batch.
pub struct PromotionImage {
    pub id: ImportId,
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub format: wgpu::TextureFormat,
}

/// Application-owned image registry called inside the native promotion transaction.
pub trait ImportedImageRegistry {
    fn install(&mut self, images: &[PromotionImage]) -> Result<()>;
    fn prune(&mut self, images: &[ImportId]);
}

/// Native DMA-BUF services supplied to an application host.
///
/// This keeps calloop channels and the Vulkan source cache behind a core-owned
/// capability instead of exposing either implementation detail to the
/// application crate.
#[derive(Clone)]
pub struct DmabufContext {
    release_sender: CalloopSender<DmabufEvent>,
    sources: DmabufSourceCache,
    capabilities: Option<DmabufCapabilities>,
}

/// Probe-only external importer whose private release channel remains alive.
///
/// The channel is deliberately not drained because a probe imports a bounded
/// number of frames and then exits. Long-lived adapters must use the host-owned
/// [`DmabufContext`] whose completion channel is drained by calloop.
pub struct ExternalDmabufImportProbe {
    context: DmabufContext,
    _release_source: CalloopChannel<DmabufEvent>,
}

impl ExternalDmabufImportProbe {
    pub fn context(&self) -> &DmabufContext {
        &self.context
    }
}

impl DmabufContext {
    pub(crate) const fn new(
        release_sender: CalloopSender<DmabufEvent>,
        sources: DmabufSourceCache,
        capabilities: Option<DmabufCapabilities>,
    ) -> Self {
        Self {
            release_sender,
            sources,
            capabilities,
        }
    }

    /// Creates a bounded probe importer on an existing Weld wgpu device.
    pub fn for_external_import_probe(
        device: &wgpu::Device,
        capabilities: DmabufCapabilities,
    ) -> ExternalDmabufImportProbe {
        let (release_sender, release_source) = calloop::channel::channel();
        ExternalDmabufImportProbe {
            context: Self::new(
                release_sender,
                DmabufSourceCache::new(device),
                Some(capabilities),
            ),
            _release_source: release_source,
        }
    }

    pub fn create_manager(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<Option<DmabufManager>> {
        DmabufManager::new(
            device,
            queue,
            self.release_sender.clone(),
            self.sources.clone(),
        )
    }

    /// Returns the selected Vulkan render node and importable format/modifier pairs.
    pub fn external_imports(&self) -> Result<Option<super::ExternalDmabufCapabilities>> {
        self.capabilities
            .as_ref()
            .map(DmabufCapabilities::external_imports)
            .transpose()
    }

    pub(crate) fn release_unrendered(&self, pending: PendingWaylandDmabufUse) {
        let _ = self
            .release_sender
            .send(DmabufEvent::LeaseCompleted(pending.release));
    }

    pub(crate) fn lease_dmabuf(
        &self,
        source: ClientSourceId,
        buffer_local: u64,
        use_local: u64,
        metadata: ClientBufferMetadata,
        pending: PendingWaylandDmabufUse,
    ) -> anyhow::Result<ClientBufferLease> {
        let PendingWaylandDmabufUse { access, release } = pending;
        if self.sources.get(&access.dmabuf).is_none() {
            let _ = self
                .release_sender
                .send(DmabufEvent::LeaseCompleted(release));
            anyhow::bail!("committed DMA-BUF was not imported during protocol creation");
        }
        let release_sender = self.release_sender.clone();
        ClientBufferLease::new(
            ClientBufferId::new(source, buffer_local),
            ClientBufferUseId::new(source, use_local),
            metadata,
            Rc::new(DirectClientBufferAccess::Dmabuf(access)),
            move |_| {
                let _ = release_sender.send(DmabufEvent::LeaseCompleted(release));
            },
        )
        .map_err(anyhow::Error::new)
    }

    pub(crate) fn import_id(&self, pending: &PendingWaylandDmabufUse) -> Option<ImportId> {
        self.sources
            .get(&pending.access.dmabuf)
            .map(|imported| imported.id)
    }

    /// Imports one descriptor-owned DMA-BUF without granting it Wayland release ownership.
    pub fn import_external(&self, external: ExternalDmabuf) -> Result<DmabufAccess> {
        let dmabuf = external.into_smithay()?;
        let capabilities = self
            .capabilities
            .as_ref()
            .context("DMA-BUF import is unavailable on this renderer")?;
        anyhow::ensure!(
            capabilities.supports(dmabuf.format()),
            "external DMA-BUF format/modifier was not advertised by this renderer"
        );
        self.sources.import(&dmabuf)?;
        Ok(DmabufAccess::new(dmabuf))
    }

    /// Creates a client-buffer lease around an already imported external DMA-BUF.
    pub fn lease_external(
        &self,
        buffer: ClientBufferId,
        use_id: ClientBufferUseId,
        metadata: ClientBufferMetadata,
        access: DmabufAccess,
        notify: impl FnOnce(ClientBufferUseId) + 'static,
    ) -> Result<ClientBufferLease> {
        self.sources
            .get(&access.dmabuf)
            .context("external DMA-BUF was not imported")?;
        ClientBufferLease::new(
            buffer,
            use_id,
            metadata,
            Rc::new(DirectClientBufferAccess::Dmabuf(access)),
            notify,
        )
        .map_err(anyhow::Error::new)
    }

    /// Removes a descriptor-owned DMA-BUF from the reusable import cache.
    pub fn remove_external(&self, access: &DmabufAccess) {
        self.sources.remove(&access.dmabuf);
    }

    /// Creates a DMA-BUF capability with no live protocol release consumer.
    ///
    /// This exists for feature-gated headless render benchmarks, which exercise
    /// the production Bevy/wgpu bridge without constructing a Wayland backend.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn for_headless_benchmark(device: &wgpu::Device) -> Self {
        let (release_sender, _release_source) = calloop::channel::channel();
        Self::new(release_sender, DmabufSourceCache::new(device), None)
    }
}

/// Owns direct client-image promotion, retirement, and GPU completion.
pub struct DmabufManager {
    device: wgpu::Device,
    queue: wgpu::Queue,
    raw_device: ash::Device,
    queue_family: u32,
    sources: DmabufSourceCache,
    layers: HashMap<SurfaceLayerKey, SurfaceLayerImages>,
    superseded: Vec<StagedImage>,
    retiring: Vec<DisplayedImage>,
    known_sources: HashMap<ImportId, Rc<ImportedDmabufSource>>,
    acquired_sources: AcquiredSources,
    pending_leases: PendingGpuLeases,
    completion_sender: mpsc::Sender<CompletionCommand>,
    completion_thread: Option<JoinHandle<()>>,
}

impl DmabufManager {
    fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        release_sender: CalloopSender<DmabufEvent>,
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
            known_sources: HashMap::new(),
            acquired_sources: AcquiredSources::default(),
            pending_leases: PendingGpuLeases::default(),
            completion_sender,
            completion_thread: Some(completion_thread),
        }))
    }

    /// Stage a fresh client-buffer identity without acquiring it for rendering yet.
    pub fn stage(
        &mut self,
        surface: SurfaceId,
        layer: SurfaceLayerId,
        lease: ClientBufferLease,
    ) -> Result<StagedImport> {
        let access = lease
            .access_rc::<DirectClientBufferAccess>()
            .context("client-buffer lease does not contain DMA-BUF access")?;
        let frame = match access.as_ref() {
            DirectClientBufferAccess::Dmabuf(frame) => Some(frame),
            DirectClientBufferAccess::Shm(_) => None,
        }
        .context("client-buffer lease does not contain DMA-BUF access")?;
        self.stage_inner(surface, layer, frame, lease)
    }

    fn stage_inner(
        &mut self,
        surface: SurfaceId,
        layer: SurfaceLayerId,
        frame: &DmabufAccess,
        lease: ClientBufferLease,
    ) -> Result<StagedImport> {
        let source = self
            .sources
            .get(&frame.dmabuf)
            .context("committed DMA-BUF was not imported during protocol creation")?;
        let id = source.id;
        self.known_sources.insert(id, source.clone());
        let extent = source.texture.size();
        let format = source.format;
        let key = SurfaceLayerKey { surface, layer };
        let layer_images = self.layers.entry(key).or_default();
        if let Some(staged) = layer_images.staged.replace(StagedImage {
            id,
            source,
            use_id: lease.use_id(),
            lease,
        }) {
            // Its owned placeholder may still be referenced by a queued ECS
            // snapshot. Keep it until the next main-world advance has drained
            // those events, then release it without a GPU ownership transfer.
            self.superseded.push(staged);
        }
        Ok(StagedImport {
            id,
            extent,
            format,
            y_inverted: frame.dmabuf.y_inverted(),
        })
    }

    pub fn staged_ids(&self) -> impl Iterator<Item = ImportId> + '_ {
        self.layers
            .values()
            .filter_map(|images| images.staged.as_ref().map(|image| image.id))
    }

    /// Promote the newest referenced buffers before Bevy records its composition.
    pub fn prepare_render(
        &mut self,
        referenced_ids: &HashSet<ImportId>,
        registry: &mut impl ImportedImageRegistry,
    ) -> Result<Vec<ImportId>> {
        let superseded = std::mem::take(&mut self.superseded);
        Self::drop_unacquired_leases(superseded);
        self.prune_dead_sources(registry);

        let staged = self
            .layers
            .iter_mut()
            .filter_map(|(key, images)| images.staged.take().map(|image| (*key, image)))
            .collect::<Vec<_>>();
        if staged.is_empty() {
            return Ok(Vec::new());
        }

        let referenced = staged
            .into_iter()
            .partition::<Vec<_>, _>(|(_, staged)| referenced_ids.contains(&staged.id));
        let (referenced, discarded) = referenced;
        let discarded = discarded
            .into_iter()
            .map(|(_, image)| image)
            .collect::<Vec<_>>();
        Self::drop_unacquired_leases(discarded);
        self.prune_dead_sources(registry);
        if referenced.is_empty() {
            return Ok(Vec::new());
        }

        let mut planned_acquires = HashSet::new();
        for (_, staged) in &referenced {
            let needs_acquire = !self.acquired_sources.contains(staged.source.image)
                && !planned_acquires.contains(&staged.source.image);
            if needs_acquire {
                planned_acquires.insert(staged.source.image);
            }
        }
        let planned_acquires = planned_acquires.into_iter().collect::<Vec<_>>();
        let acquire_command = match self.external_barrier_command(&planned_acquires, true) {
            Ok(command) => command,
            Err(error) => {
                let failed = referenced
                    .into_iter()
                    .map(|(_, image)| image)
                    .collect::<Vec<_>>();
                Self::drop_unacquired_leases(failed);
                self.prune_dead_sources(registry);
                return Err(error).context("could not acquire the complete staged DMA-BUF batch");
            }
        };
        let promotion_images = referenced
            .iter()
            .map(|(_, staged)| staged)
            .fold(HashMap::new(), |mut images, staged| {
                images.entry(staged.id).or_insert_with(|| PromotionImage {
                    id: staged.id,
                    texture: staged.source.texture.clone(),
                    view: staged.source.view.clone(),
                    format: staged.source.format,
                });
                images
            })
            .into_values()
            .collect::<Vec<_>>();
        if let Err(error) = registry.install(&promotion_images) {
            let failed = referenced
                .into_iter()
                .map(|(_, image)| image)
                .collect::<Vec<_>>();
            Self::drop_unacquired_leases(failed);
            self.prune_dead_sources(registry);
            return Err(error).context("could not install the complete staged DMA-BUF batch");
        }

        if let Some(command) = acquire_command {
            self.queue.submit([command]);
        }
        let promoted = referenced
            .iter()
            .map(|(_, staged)| staged.id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        for (key, staged) in referenced {
            self.acquired_sources.retain(staged.source.image);
            let images = self.layers.entry(key).or_default();
            if let Some(displayed) = images.displayed.replace(DisplayedImage {
                id: staged.id,
                source: staged.source,
                use_id: staged.use_id,
                lease: staged.lease,
            }) {
                self.retiring.push(displayed);
            }
            debug!(?key.surface, ?key.layer, "promoted direct DMA-BUF image");
        }
        Ok(promoted)
    }

    /// Retire replaced buffers after Bevy has submitted every possible old-image read.
    pub fn finish_render(&mut self, registry: &mut impl ImportedImageRegistry) -> Result<()> {
        if self.retiring.is_empty() {
            return Ok(());
        }
        let retirement = self
            .acquired_sources
            .plan_retirements(self.retiring.iter().map(|image| image.source.image))?;
        let release_command = self.external_barrier_command(&retirement.releases, false)?;
        self.acquired_sources.commit_retirements(&retirement);
        let retiring = std::mem::take(&mut self.retiring);

        let submission = self.queue.submit(release_command);
        let work = CompletionWork {
            submission,
            uses: retiring.iter().map(|image| image.use_id).collect(),
            _textures: retiring
                .iter()
                .map(|image| image.source.texture.clone())
                .collect(),
        };
        for image in retiring {
            self.pending_leases.retain(image.use_id, image.lease);
        }
        self.queue_completion(work);
        self.prune_dead_sources(registry);
        Ok(())
    }

    fn drop_unacquired_leases(images: Vec<StagedImage>) {
        drop(images);
    }

    fn queue_completion(&mut self, work: CompletionWork) {
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
            for use_id in work.uses {
                self.pending_leases.complete(use_id);
            }
        }
    }

    pub fn complete_gpu_uses(&mut self, uses: &[ClientBufferUseId]) {
        for use_id in uses {
            self.pending_leases.complete(*use_id);
        }
    }

    fn external_barrier_command(
        &self,
        images: &[vk::Image],
        acquire: bool,
    ) -> Result<Option<wgpu::CommandBuffer>> {
        if images.is_empty() {
            return Ok(None);
        }
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

    fn prune_dead_sources(&mut self, registry: &mut impl ImportedImageRegistry) {
        let retained = self
            .layers
            .values()
            .flat_map(|images| {
                images
                    .staged
                    .iter()
                    .map(|image| image.id)
                    .chain(images.displayed.iter().map(|image| image.id))
            })
            .chain(self.superseded.iter().map(|image| image.id))
            .chain(self.retiring.iter().map(|image| image.id))
            .collect::<HashSet<_>>();
        let pruned = self
            .known_sources
            .iter()
            .filter_map(|(&id, source)| {
                (!source.alive.get() && !retained.contains(&id)).then_some(id)
            })
            .collect::<Vec<_>>();
        registry.prune(&pruned);
        for id in pruned {
            self.known_sources.remove(&id);
        }
    }

    pub fn retain_surface_layers(
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

    pub fn remove_surface(&mut self, surface: SurfaceId) {
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

    pub fn remove_layer(&mut self, surface: SurfaceId, layer: SurfaceLayerId) {
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
            drop(image);
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
        let command = match self.external_barrier_command(&acquired_images, false) {
            Ok(command) => command,
            Err(error) => {
                warn!(%error, "could not release acquired DMA-BUFs during shutdown");
                return;
            }
        };
        let submission = self.queue.submit(command);
        self.acquired_sources.clear();
        let work = CompletionWork {
            submission,
            uses: displayed.iter().map(|image| image.use_id).collect(),
            _textures: displayed
                .iter()
                .map(|image| image.source.texture.clone())
                .collect(),
        };
        for image in displayed {
            self.pending_leases.retain(image.use_id, image.lease);
        }
        self.queue_completion(work);
    }
}

impl Drop for DmabufManager {
    fn drop(&mut self) {
        self.submit_shutdown_releases();
        let _ = self.completion_sender.send(CompletionCommand::Shutdown);
        if let Some(thread) = self.completion_thread.take()
            && thread.join().is_err()
        {
            error!("DMA-BUF completion worker panicked during shutdown");
        }
        self.pending_leases.clear();
    }
}

fn completion_loop(
    device: wgpu::Device,
    release_sender: CalloopSender<DmabufEvent>,
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
            warn!(%error, uses = ?work.uses, "DMA-BUF GPU completion wait failed; releasing client buffers during recovery");
        }
        for use_id in work.uses {
            if release_sender
                .send(DmabufEvent::GpuUseCompleted(use_id))
                .is_err()
            {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use ash::vk::Handle;
    use weld_client::{
        ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, ClientSourceId,
        Extent,
    };

    use super::{AcquiredSources, PendingGpuLeases};

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

    #[test]
    fn gpu_completion_drops_only_the_manager_consumer_of_a_shared_lease() {
        let completed = Rc::new(Cell::new(0));
        let completed_for_callback = completed.clone();
        let source = ClientSourceId::new(1);
        let lease = ClientBufferLease::new(
            ClientBufferId::new(source, 1),
            ClientBufferUseId::new(source, 1),
            ClientBufferMetadata::new(Extent::new(1, 1), false),
            Rc::new(()),
            move |_| completed_for_callback.set(completed_for_callback.get() + 1),
        )
        .expect("matching source");
        let exporter = lease.clone();
        let mut pending = PendingGpuLeases::default();
        let use_id = lease.use_id();
        pending.retain(use_id, lease);

        pending.complete(use_id);
        assert_eq!(completed.get(), 0);

        drop(exporter);
        assert_eq!(completed.get(), 1);
    }

    #[test]
    fn gpu_completion_ids_keep_adapter_namespaces_distinct() {
        let completed = Rc::new(Cell::new(0));
        let mut pending = PendingGpuLeases::default();
        for source in [ClientSourceId::new(1), ClientSourceId::new(2)] {
            let completed = completed.clone();
            let use_id = ClientBufferUseId::new(source, 1);
            let lease = ClientBufferLease::new(
                ClientBufferId::new(source, 1),
                use_id,
                ClientBufferMetadata::new(Extent::new(1, 1), false),
                Rc::new(()),
                move |_| completed.set(completed.get() + 1),
            )
            .expect("matching source");
            pending.retain(use_id, lease);
        }

        pending.complete(ClientBufferUseId::new(ClientSourceId::new(1), 1));
        assert_eq!(completed.get(), 1);
        pending.complete(ClientBufferUseId::new(ClientSourceId::new(2), 1));
        assert_eq!(completed.get(), 2);
    }
}
