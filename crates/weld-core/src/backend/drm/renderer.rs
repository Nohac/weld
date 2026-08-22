//! Minimal Smithay renderer that makes a leased primary buffer a Bevy target.

use std::{
    collections::{HashMap, hash_map::Entry},
    error::Error,
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use ash::vk;
use smithay::{
    backend::{
        allocator::{
            Buffer, Format, Fourcc, Modifier,
            dmabuf::{Dmabuf, WeakDmabuf},
            format::FormatSet,
        },
        renderer::{
            Bind, Color32F, ContextId, DebugFlags, Frame, ImportMem, Renderer, RendererSuper,
            Texture, TextureFilter,
            element::{Element, Id, Kind, RenderElement},
            sync::SyncPoint,
            utils::{CommitCounter, OpaqueRegions},
        },
    },
    render_elements,
    utils::{
        Buffer as BufferCoord, Physical, Rectangle, Scale, Size, Transform, user_data::UserDataMap,
    },
};

use crate::renderer::CompositionBlitter;
use crate::{
    OutputId,
    host::{
        CompositionDestination, CompositionHost, CompositionOutputFrame, CompositionOutputRequest,
        CompositionTargetView,
    },
    surface::Extent,
};

use super::vulkan::{ForeignImageBarrier, foreign_image_barrier_command};

const SCANOUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;

#[derive(Debug)]
pub(super) struct DrmRenderError(String);

impl DrmRenderError {
    fn message(message: impl fmt::Display) -> Self {
        Self(message.to_string())
    }
}

impl fmt::Display for DrmRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for DrmRenderError {}

#[derive(Clone)]
pub(super) enum DrmTexture {
    Composition { output: OutputId, extent: Extent },
    Memory(Arc<MemoryTexture>),
}

impl fmt::Debug for DrmTexture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Composition { output, extent } => formatter
                .debug_struct("Composition")
                .field("output", output)
                .field("extent", extent)
                .finish(),
            Self::Memory(texture) => formatter
                .debug_struct("Memory")
                .field("extent", &texture.extent)
                .finish(),
        }
    }
}

impl Texture for DrmTexture {
    fn width(&self) -> u32 {
        self.extent().width
    }

    fn height(&self) -> u32 {
        self.extent().height
    }

    fn format(&self) -> Option<Fourcc> {
        Some(Fourcc::Argb8888)
    }
}

impl DrmTexture {
    fn extent(&self) -> Extent {
        match self {
            Self::Composition { extent, .. } => *extent,
            Self::Memory(texture) => texture.extent,
        }
    }
}

pub(super) struct MemoryTexture {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    extent: Extent,
}

struct ImportedScanout {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    image: vk::Image,
    used: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RenderBatchMode {
    #[default]
    Closed,
    Open,
}

#[derive(Default)]
struct RenderBatch {
    mode: RenderBatchMode,
    requests: Vec<CompositionOutputRequest>,
    frames: Vec<CompositionOutputFrame>,
    pending: Vec<PendingFrame>,
    composition_drawn: HashMap<OutputId, bool>,
    last_gpu_wait: Duration,
}

struct PendingFrame {
    output: OutputId,
    encoder: Option<wgpu::CommandEncoder>,
    image: vk::Image,
    composition_drawn: bool,
}

pub(super) enum BatchFinish {
    Complete,
    CompositionFailed(anyhow::Error),
}

impl RenderBatch {
    fn open(&mut self) -> Result<()> {
        if self.mode != RenderBatchMode::Closed || !self.pending.is_empty() {
            bail!("a DRM render batch is already open");
        }
        self.requests.clear();
        self.frames.clear();
        self.composition_drawn.clear();
        self.last_gpu_wait = Duration::ZERO;
        self.mode = RenderBatchMode::Open;
        Ok(())
    }

    fn register_composition(
        &mut self,
        output: OutputId,
        target: wgpu::TextureView,
        extent: Extent,
    ) -> Result<()> {
        if self.mode != RenderBatchMode::Open {
            bail!("Bevy composition requires an open DRM render batch");
        }
        if self.requests.iter().any(|request| request.output == output) {
            bail!("DRM render batch requested output {output:?} more than once");
        }
        self.requests.push(CompositionOutputRequest {
            output,
            destination: CompositionDestination::External(CompositionTargetView::new(
                target,
                extent,
                SCANOUT_FORMAT,
            )),
        });
        Ok(())
    }

    fn defer(&mut self, frame: PendingFrame) -> Result<()> {
        if self.mode != RenderBatchMode::Open {
            bail!("cannot defer a DRM frame outside a render batch");
        }
        self.pending.push(frame);
        Ok(())
    }

    fn finish(
        &mut self,
        host: &mut dyn CompositionHost,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        raw_device: &ash::Device,
        queue_family: u32,
    ) -> Result<BatchFinish> {
        if self.mode != RenderBatchMode::Open {
            bail!("cannot finish a closed DRM render batch");
        }
        let render_result = if self.requests.is_empty() {
            Ok(())
        } else {
            host.render_outputs(&self.requests, &mut self.frames)
                .and_then(|()| validate_composition_frames(&self.requests, &self.frames))
        };
        let release_result = self.submit_pending(device, queue, raw_device, queue_family);
        self.close();
        match (render_result, release_result) {
            (_, Err(error)) => Err(error),
            (Err(error), Ok(())) => Ok(BatchFinish::CompositionFailed(error)),
            (Ok(()), Ok(())) => Ok(BatchFinish::Complete),
        }
    }

    fn abort(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        raw_device: &ash::Device,
        queue_family: u32,
    ) -> Result<()> {
        if self.mode == RenderBatchMode::Closed && self.pending.is_empty() {
            return Ok(());
        }
        let result = self.submit_pending(device, queue, raw_device, queue_family);
        self.close();
        result
    }

    fn submit_pending(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        raw_device: &ash::Device,
        queue_family: u32,
    ) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut commands = Vec::with_capacity(self.pending.len() * 2);
        for pending in self.pending.drain(..) {
            if let Some(encoder) = pending.encoder {
                commands.push(encoder.finish());
            }
            // SAFETY: the scanout import remains cached through this batch and
            // all rendering and release barriers use the same wgpu queue.
            let release = unsafe {
                foreign_image_barrier_command(
                    device,
                    raw_device,
                    queue_family,
                    pending.image,
                    true,
                    ForeignImageBarrier::Release,
                )
            }?;
            commands.push(release);
            self.composition_drawn
                .insert(pending.output, pending.composition_drawn);
        }
        let submission = queue.submit(commands);
        let wait_started = Instant::now();
        device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: None,
            })
            .map(|_| ())?;
        self.last_gpu_wait = wait_started.elapsed();
        Ok(())
    }

    fn close(&mut self) {
        self.mode = RenderBatchMode::Closed;
        self.requests.clear();
        self.frames.clear();
    }
}

fn validate_composition_frames(
    requests: &[CompositionOutputRequest],
    frames: &[CompositionOutputFrame],
) -> Result<()> {
    if frames.len() != requests.len() {
        bail!("Bevy returned the wrong number of DRM compositions");
    }
    for (request, frame) in requests.iter().zip(frames) {
        if frame.output != request.output || frame.frame.owned_texture().is_some() {
            bail!("Bevy returned the wrong DRM composition destination");
        }
    }
    Ok(())
}

pub(super) struct DrmRenderState {
    device: wgpu::Device,
    queue: wgpu::Queue,
    raw_device: ash::Device,
    queue_family: u32,
    context: ContextId<DrmTexture>,
    debug_flags: DebugFlags,
    formats: Vec<Format>,
    imports: HashMap<WeakDmabuf, ImportedScanout>,
    blitter: CompositionBlitter,
    batch: RenderBatch,
    last_gpu_wait: Duration,
    composition_drawn: HashMap<OutputId, bool>,
}

impl fmt::Debug for DrmRenderState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DrmRenderState")
            .field("queue_family", &self.queue_family)
            .field("context", &self.context)
            .field("formats", &self.formats)
            .field("imports", &self.imports.len())
            .finish_non_exhaustive()
    }
}

impl DrmRenderState {
    pub(super) fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        formats: Vec<Format>,
    ) -> Result<Self> {
        // SAFETY: these immutable Vulkan handles remain valid while this state
        // retains the owning wgpu device.
        let (raw_device, queue_family) = unsafe {
            let raw = device
                .as_hal::<wgpu::hal::api::Vulkan>()
                .context("DRM render device is not backed by Vulkan")?;
            (raw.raw_device().clone(), raw.queue_family_index())
        };
        let blitter = CompositionBlitter::new(&device, SCANOUT_FORMAT);
        Ok(Self {
            device,
            queue,
            raw_device,
            queue_family,
            context: ContextId::new(),
            debug_flags: DebugFlags::empty(),
            formats,
            imports: HashMap::new(),
            blitter,
            batch: RenderBatch::default(),
            last_gpu_wait: Duration::ZERO,
            composition_drawn: HashMap::new(),
        })
    }

    pub(super) fn renderer(&mut self, output: OutputId) -> DrmRenderer<'_> {
        self.imports.retain(|dmabuf, _| !dmabuf.is_gone());
        DrmRenderer {
            state: self,
            output,
        }
    }

    pub(super) fn begin_batch(&mut self) -> Result<()> {
        self.last_gpu_wait = Duration::ZERO;
        self.composition_drawn.clear();
        self.batch.open()
    }

    pub(super) fn finish_batch(&mut self, host: &mut dyn CompositionHost) -> Result<BatchFinish> {
        let result = self.batch.finish(
            host,
            &self.device,
            &self.queue,
            &self.raw_device,
            self.queue_family,
        );
        self.last_gpu_wait = self.batch.last_gpu_wait;
        self.composition_drawn
            .extend(self.batch.composition_drawn.drain());
        result
    }

    pub(super) fn abort_batch(&mut self) {
        if let Err(error) = self.batch.abort(
            &self.device,
            &self.queue,
            &self.raw_device,
            self.queue_family,
        ) {
            tracing::warn!(%error, "failed to release an aborted DRM render batch");
        }
    }

    pub(super) fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub(super) fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    pub(super) const fn last_gpu_wait(&self) -> Duration {
        self.last_gpu_wait
    }

    pub(super) fn composition_drawn(&self, output: OutputId) -> bool {
        self.composition_drawn
            .get(&output)
            .copied()
            .unwrap_or(false)
    }
}

pub(super) struct DrmRenderer<'a> {
    state: &'a mut DrmRenderState,
    output: OutputId,
}

impl fmt::Debug for DrmRenderer<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DrmRenderer")
            .field("output", &self.output)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub(super) struct DrmFramebuffer {
    key: WeakDmabuf,
    view: wgpu::TextureView,
    image: vk::Image,
    size: Size<i32, Physical>,
    format: Fourcc,
}

impl Texture for DrmFramebuffer {
    fn width(&self) -> u32 {
        u32::try_from(self.size.w).unwrap_or_default()
    }

    fn height(&self) -> u32 {
        u32::try_from(self.size.h).unwrap_or_default()
    }

    fn format(&self) -> Option<Fourcc> {
        Some(self.format)
    }
}

impl<'host> RendererSuper for DrmRenderer<'host> {
    type Error = DrmRenderError;
    type TextureId = DrmTexture;
    type Framebuffer<'buffer> = DrmFramebuffer;
    type Frame<'frame, 'buffer>
        = DrmFrame<'frame>
    where
        'buffer: 'frame,
        Self: 'frame;
}

impl<'host> Renderer for DrmRenderer<'host> {
    fn context_id(&self) -> ContextId<Self::TextureId> {
        self.state.context.clone()
    }

    fn downscale_filter(&mut self, _filter: TextureFilter) -> Result<(), Self::Error> {
        Ok(())
    }

    fn upscale_filter(&mut self, _filter: TextureFilter) -> Result<(), Self::Error> {
        Ok(())
    }

    fn set_debug_flags(&mut self, flags: DebugFlags) {
        self.state.debug_flags = flags;
    }

    fn debug_flags(&self) -> DebugFlags {
        self.state.debug_flags
    }

    fn render<'frame, 'buffer>(
        &'frame mut self,
        framebuffer: &'frame mut Self::Framebuffer<'buffer>,
        output_size: Size<i32, Physical>,
        dst_transform: Transform,
    ) -> Result<Self::Frame<'frame, 'buffer>, Self::Error>
    where
        'buffer: 'frame,
    {
        if dst_transform != Transform::Normal || framebuffer.size != output_size {
            return Err(DrmRenderError::message(
                "DRM renderer supports only a normal full-size output target",
            ));
        }
        let previously_used = self
            .state
            .imports
            .get(&framebuffer.key)
            .map(|imported| imported.used)
            .ok_or_else(|| DrmRenderError::message("scanout import disappeared before render"))?;
        // SAFETY: the import cache retains the image through completion, and
        // this renderer owns acquire, render, and release on one queue.
        let acquire = unsafe {
            foreign_image_barrier_command(
                &self.state.device,
                &self.state.raw_device,
                self.state.queue_family,
                framebuffer.image,
                previously_used,
                ForeignImageBarrier::Acquire,
            )
        }
        .map_err(DrmRenderError::message)?;
        self.state.queue.submit([acquire]);
        self.state
            .imports
            .get_mut(&framebuffer.key)
            .ok_or_else(|| DrmRenderError::message("scanout import disappeared before render"))?
            .used = true;
        Ok(DrmFrame {
            device: &self.state.device,
            queue: &self.state.queue,
            raw_device: &self.state.raw_device,
            queue_family: self.state.queue_family,
            context: self.state.context.clone(),
            blitter: &self.state.blitter,
            target: framebuffer.view.clone(),
            image: framebuffer.image,
            output_size,
            output: self.output,
            batch: &mut self.state.batch,
            last_gpu_wait: &mut self.state.last_gpu_wait,
            encoder: Some(self.state.device.create_command_encoder(
                &wgpu::CommandEncoderDescriptor {
                    label: Some("Weld DRM overlay encoder"),
                },
            )),
            pre_composition_commands: false,
            composition_drawn: false,
            released: false,
        })
    }

    fn wait(&mut self, sync: &SyncPoint) -> Result<(), Self::Error> {
        sync.wait().map_err(DrmRenderError::message)
    }

    fn cleanup_texture_cache(&mut self) -> Result<(), Self::Error> {
        self.state.imports.retain(|dmabuf, _| !dmabuf.is_gone());
        Ok(())
    }
}

impl<'host> Bind<Dmabuf> for DrmRenderer<'host> {
    fn bind<'a>(&mut self, target: &'a mut Dmabuf) -> Result<Self::Framebuffer<'a>, Self::Error> {
        validate_scanout_target(target).map_err(DrmRenderError::message)?;
        if !self.state.formats.contains(&target.format()) {
            return Err(DrmRenderError::message(format_args!(
                "Smithay selected unsupported scanout format {:?}",
                target.format()
            )));
        }
        let key = target.weak();
        if let Entry::Vacant(entry) = self.state.imports.entry(key.clone()) {
            entry.insert(
                import_scanout(&self.state.device, target).map_err(DrmRenderError::message)?,
            );
        }
        let imported = self
            .state
            .imports
            .get(&key)
            .ok_or_else(|| DrmRenderError::message("scanout import was not cached"))?;
        Ok(DrmFramebuffer {
            key,
            view: imported.view.clone(),
            image: imported.image,
            size: Size::from((target.size().w, target.size().h)),
            format: target.format().code,
        })
    }

    fn supported_formats(&self) -> Option<FormatSet> {
        Some(self.state.formats.iter().copied().collect())
    }
}

impl<'host> ImportMem for DrmRenderer<'host> {
    fn import_memory(
        &mut self,
        data: &[u8],
        format: Fourcc,
        size: Size<i32, BufferCoord>,
        flipped: bool,
    ) -> Result<Self::TextureId, Self::Error> {
        if format != Fourcc::Argb8888 || flipped || size.w <= 0 || size.h <= 0 {
            return Err(DrmRenderError::message(
                "cursor memory must be non-flipped ARGB8888 with positive dimensions",
            ));
        }
        let width = u32::try_from(size.w).map_err(DrmRenderError::message)?;
        let height = u32::try_from(size.h).map_err(DrmRenderError::message)?;
        let extent = Extent::new(width, height);
        validate_memory_size(data, extent)?;
        let texture = self.state.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Weld DRM cursor texture"),
            size: wgpu_extent(extent),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: SCANOUT_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        write_memory(&self.state.queue, &texture, data, extent);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.state.blitter.create_bind_group(
            &self.state.device,
            "Weld DRM cursor bind group",
            &view,
        );
        Ok(DrmTexture::Memory(Arc::new(MemoryTexture {
            _texture: texture,
            bind_group,
            extent,
        })))
    }

    fn update_memory(
        &mut self,
        texture: &Self::TextureId,
        data: &[u8],
        _region: Rectangle<i32, BufferCoord>,
    ) -> Result<(), Self::Error> {
        let DrmTexture::Memory(texture) = texture else {
            return Err(DrmRenderError::message(
                "cannot upload memory into the Weld composition marker",
            ));
        };
        validate_memory_size(data, texture.extent)?;
        write_memory(&self.state.queue, &texture._texture, data, texture.extent);
        Ok(())
    }

    fn mem_formats(&self) -> Box<dyn Iterator<Item = Fourcc>> {
        Box::new(std::iter::once(Fourcc::Argb8888))
    }
}

pub(super) struct DrmFrame<'frame> {
    device: &'frame wgpu::Device,
    queue: &'frame wgpu::Queue,
    raw_device: &'frame ash::Device,
    queue_family: u32,
    context: ContextId<DrmTexture>,
    blitter: &'frame CompositionBlitter,
    target: wgpu::TextureView,
    image: vk::Image,
    output_size: Size<i32, Physical>,
    output: OutputId,
    batch: &'frame mut RenderBatch,
    last_gpu_wait: &'frame mut Duration,
    encoder: Option<wgpu::CommandEncoder>,
    pre_composition_commands: bool,
    composition_drawn: bool,
    released: bool,
}

impl DrmFrame<'_> {
    fn release_command(&self) -> Result<wgpu::CommandBuffer, DrmRenderError> {
        // SAFETY: the scanout cache retains this image, and release is submitted
        // on the same queue after every command that can access the image.
        unsafe {
            foreign_image_barrier_command(
                self.device,
                self.raw_device,
                self.queue_family,
                self.image,
                true,
                ForeignImageBarrier::Release,
            )
        }
        .map_err(DrmRenderError::message)
    }

    fn submit_and_wait(&mut self) -> Result<(), DrmRenderError> {
        let release = self.release_command()?;
        let commands = self
            .encoder
            .take()
            .map(wgpu::CommandEncoder::finish)
            .into_iter()
            .chain(std::iter::once(release));
        let submission = self.queue.submit(commands);
        let wait_started = Instant::now();
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: None,
            })
            .map(|_| ())
            .map_err(DrmRenderError::message)?;
        *self.last_gpu_wait = wait_started.elapsed();
        self.released = true;
        Ok(())
    }

    fn finish_or_defer(&mut self) -> Result<(), DrmRenderError> {
        if self.batch.mode == RenderBatchMode::Closed {
            return self.submit_and_wait();
        }
        let pending = PendingFrame {
            output: self.output,
            encoder: self.encoder.take(),
            image: self.image,
            composition_drawn: self.composition_drawn,
        };
        self.batch.defer(pending).map_err(DrmRenderError::message)?;
        self.released = true;
        Ok(())
    }

    fn render_composition(
        &mut self,
        output: OutputId,
        extent: Extent,
    ) -> Result<(), DrmRenderError> {
        if self.composition_drawn || output != self.output {
            return Err(DrmRenderError::message(
                "Weld composition marker was duplicated or targeted the wrong output",
            ));
        }
        if extent.width != self.output_size.w as u32 || extent.height != self.output_size.h as u32 {
            return Err(DrmRenderError::message(
                "Weld composition marker does not cover the leased output",
            ));
        }
        if self.pre_composition_commands {
            let encoder = self
                .encoder
                .take()
                .ok_or_else(|| DrmRenderError::message("DRM frame was already submitted"))?;
            self.queue.submit([encoder.finish()]);
            self.encoder = Some(self.device.create_command_encoder(
                &wgpu::CommandEncoderDescriptor {
                    label: Some("Weld DRM overlay encoder"),
                },
            ));
            self.pre_composition_commands = false;
        }
        self.batch
            .register_composition(output, self.target.clone(), extent)
            .map_err(DrmRenderError::message)?;
        self.composition_drawn = true;
        Ok(())
    }

    fn draw_cursor(
        &mut self,
        texture: &MemoryTexture,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), DrmRenderError> {
        let Some(encoder) = self.encoder.as_mut() else {
            return Err(DrmRenderError::message("DRM frame was already submitted"));
        };
        if dst.size.w <= 0 || dst.size.h <= 0 || damage.is_empty() {
            return Ok(());
        }
        let scissors = damage.iter().map(|rect| {
            (
                dst.loc.x.saturating_add(rect.loc.x).max(0) as u32,
                dst.loc.y.saturating_add(rect.loc.y).max(0) as u32,
                u32::try_from(rect.size.w.max(0)).unwrap_or_default(),
                u32::try_from(rect.size.h.max(0)).unwrap_or_default(),
            )
        });
        self.blitter.encode_overlay(
            encoder,
            "Weld DRM cursor fallback pass",
            &self.target,
            &texture.bind_group,
            (
                dst.loc.x as f32,
                dst.loc.y as f32,
                dst.size.w as f32,
                dst.size.h as f32,
            ),
            scissors,
        );
        Ok(())
    }
}

impl Drop for DrmFrame<'_> {
    fn drop(&mut self) {
        if !self.released
            && let Err(error) = self.finish_or_defer()
        {
            tracing::warn!(%error, "failed to release an aborted DRM frame");
        }
    }
}

impl Frame for DrmFrame<'_> {
    type Error = DrmRenderError;
    type TextureId = DrmTexture;

    fn context_id(&self) -> ContextId<Self::TextureId> {
        self.context.clone()
    }

    fn clear(
        &mut self,
        color: Color32F,
        at: &[Rectangle<i32, Physical>],
    ) -> Result<(), Self::Error> {
        if at.is_empty() {
            return Ok(());
        }
        let Some(encoder) = self.encoder.as_mut() else {
            return Err(DrmRenderError::message("DRM frame was already submitted"));
        };
        let full = Rectangle::from_size(self.output_size);
        if !at.contains(&full) {
            return Err(DrmRenderError::message(
                "partial clear is outside the initial DRM renderer contract",
            ));
        }
        let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Weld DRM full clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: f64::from(color.r()),
                        g: f64::from(color.g()),
                        b: f64::from(color.b()),
                        a: f64::from(color.a()),
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        self.pre_composition_commands = true;
        Ok(())
    }

    fn draw_solid(
        &mut self,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        color: Color32F,
    ) -> Result<(), Self::Error> {
        if dst == Rectangle::from_size(self.output_size) {
            self.clear(color, damage)
        } else {
            Err(DrmRenderError::message(
                "partial solid elements are outside the initial DRM renderer contract",
            ))
        }
    }

    fn render_texture_from_to(
        &mut self,
        texture: &Self::TextureId,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        src_transform: Transform,
        alpha: f32,
    ) -> Result<(), Self::Error> {
        match texture {
            DrmTexture::Composition { output, extent } => {
                if dst != Rectangle::from_size(self.output_size)
                    || src_transform != Transform::Normal
                    || alpha != 1.0
                {
                    return Err(DrmRenderError::message(
                        "Weld composition must be an opaque normal full-output element",
                    ));
                }
                self.render_composition(*output, *extent)
            }
            DrmTexture::Memory(texture) => {
                let full_source = Rectangle::from_size(Size::from((
                    f64::from(texture.extent.width),
                    f64::from(texture.extent.height),
                )));
                if src != full_source || src_transform != Transform::Normal || alpha != 1.0 {
                    return Err(DrmRenderError::message(
                        "cursor fallback requires a full normal opaque source",
                    ));
                }
                self.draw_cursor(texture, dst, damage)
            }
        }
    }

    fn transformation(&self) -> Transform {
        Transform::Normal
    }

    fn output_size(&self) -> Size<i32, Physical> {
        self.output_size
    }

    fn wait(&mut self, sync: &SyncPoint) -> Result<(), Self::Error> {
        sync.wait().map_err(DrmRenderError::message)
    }

    fn finish(mut self) -> Result<SyncPoint, Self::Error> {
        self.finish_or_defer()?;
        Ok(SyncPoint::signaled())
    }
}

#[derive(Debug, Clone)]
pub(super) struct CompositionElement {
    id: Id,
    texture: DrmTexture,
    geometry: Rectangle<i32, Physical>,
    commit: CommitCounter,
}

impl CompositionElement {
    pub(super) fn new(output: OutputId, extent: Extent) -> Result<Self> {
        let width = i32::try_from(extent.width).context("composition width exceeds i32")?;
        let height = i32::try_from(extent.height).context("composition height exceeds i32")?;
        Ok(Self {
            id: Id::new(),
            texture: DrmTexture::Composition { output, extent },
            geometry: Rectangle::from_size((width, height).into()),
            commit: CommitCounter::default(),
        })
    }

    pub(super) fn mark_dirty(&mut self) {
        self.commit.increment();
    }
}

impl Element for CompositionElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, BufferCoord> {
        Rectangle::from_size(Size::from((
            f64::from(self.geometry.size.w),
            f64::from(self.geometry.size.h),
        )))
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::from_slice(&[Rectangle::from_size(self.geometry.size)])
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl<R> RenderElement<R> for CompositionElement
where
    R: Renderer<TextureId = DrmTexture>,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            1.0,
        )
    }
}

// Rust has no stable trait aliases; this names the exact Smithay renderer
// contract shared by Weld's two output element variants.
pub(super) trait OutputRenderer: ImportMem + Renderer<TextureId = DrmTexture> {}

impl<R> OutputRenderer for R where R: ImportMem + Renderer<TextureId = DrmTexture> {}

render_elements! {
    pub(super) OutputElement<R> where R: OutputRenderer;
    Composition=CompositionElement,
    Cursor=smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement<R>,
}

fn validate_scanout_target(dmabuf: &Dmabuf) -> Result<()> {
    if dmabuf.num_planes() != 1 {
        bail!("DRM scanout requires exactly one DMA-BUF plane");
    }
    if dmabuf.format().modifier == Modifier::Invalid {
        bail!("DRM scanout refuses implicit DMA-BUF modifiers");
    }
    Ok(())
}

fn import_scanout(device: &wgpu::Device, dmabuf: &Dmabuf) -> Result<ImportedScanout> {
    let size = dmabuf.size();
    let width = u32::try_from(size.w).context("negative GBM buffer width")?;
    let height = u32::try_from(size.h).context("negative GBM buffer height")?;
    let fd = dmabuf
        .handles()
        .next()
        .context("DMA-BUF has no plane")?
        .try_clone_to_owned()
        .context("failed to duplicate GBM buffer fd")?;
    let stride = u64::from(dmabuf.strides().next().context("DMA-BUF has no stride")?);
    let offset = u64::from(dmabuf.offsets().next().context("DMA-BUF has no offset")?);
    let extent = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let hal_descriptor = wgpu::hal::TextureDescriptor {
        label: Some("Weld Smithay scanout import"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: SCANOUT_FORMAT,
        usage: wgpu::TextureUses::COLOR_TARGET,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: Vec::new(),
    };
    // SAFETY: capability discovery and Smithay selected this exact explicit,
    // single-plane modifier for scanout and Vulkan color-attachment use.
    let hal_texture = unsafe {
        let raw = device
            .as_hal::<wgpu::hal::api::Vulkan>()
            .context("scanout device is not backed by Vulkan")?;
        raw.texture_from_dmabuf_fd(
            fd,
            &hal_descriptor,
            dmabuf.format().modifier.into(),
            stride,
            offset,
        )?
    };
    let descriptor = wgpu::TextureDescriptor {
        label: Some("Weld Smithay scanout import"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: SCANOUT_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    };
    // SAFETY: the HAL texture was created by this exact device and descriptor.
    let texture = unsafe {
        device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(
            hal_texture,
            &descriptor,
            wgpu::TextureUses::COLOR_TARGET,
        )
    };
    // SAFETY: the texture is retained in the import cache while this raw image
    // handle participates in ownership barriers.
    let image = unsafe {
        texture
            .as_hal::<wgpu::hal::api::Vulkan>()
            .context("imported scanout texture is not backed by Vulkan")?
            .raw_handle()
    };
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    tracing::info!(format = ?dmabuf.format(), size = ?dmabuf.size(), "imported Smithay-owned scanout buffer");
    Ok(ImportedScanout {
        _texture: texture,
        view,
        image,
        used: false,
    })
}

fn validate_memory_size(data: &[u8], extent: Extent) -> Result<(), DrmRenderError> {
    let required = usize::try_from(extent.width)
        .ok()
        .and_then(|width| width.checked_mul(extent.height as usize))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| DrmRenderError::message("cursor memory dimensions overflow"))?;
    if data.len() < required {
        return Err(DrmRenderError::message(
            "cursor memory is shorter than its extent",
        ));
    }
    Ok(())
}

fn write_memory(queue: &wgpu::Queue, texture: &wgpu::Texture, data: &[u8], extent: Extent) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(extent.width * 4),
            rows_per_image: Some(extent.height),
        },
        wgpu_extent(extent),
    );
}

const fn wgpu_extent(extent: Extent) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width: extent.width,
        height: extent.height,
        depth_or_array_layers: 1,
    }
}
