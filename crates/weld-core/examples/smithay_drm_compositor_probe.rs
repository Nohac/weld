//! Validate Smithay-owned GBM/KMS presentation with direct wgpu rendering.
//!
//! This is deliberately not a general Smithay renderer. It supports only the
//! solid-color path needed to prove that `DrmOutputManager` may own the output
//! allocation and KMS lifecycle while wgpu writes directly into that buffer.

use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    error::Error,
    fmt,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use ash::vk;
use calloop::{
    EventLoop,
    signals::{Signal, Signals},
};
use clap::Parser;
use smithay::{
    backend::{
        allocator::{
            Buffer, Format, Fourcc, Modifier,
            dmabuf::{Dmabuf, WeakDmabuf},
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata,
            compositor::{FrameFlags, PrimaryPlaneElement},
            exporter::gbm::{GbmFramebufferExporter, NodeFilter},
            output::{DrmOutputManager, DrmOutputRenderElements},
        },
        renderer::{
            Bind, Color32F, ContextId, DebugFlags, Frame, Renderer, RendererSuper, Texture,
            TextureFilter,
            element::{
                Kind,
                solid::{SolidColorBuffer, SolidColorRenderElement},
            },
            sync::SyncPoint,
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, primary_gpu},
    },
    output::OutputModeSource,
    reexports::{
        drm::control::{Mode, ModeTypeFlags, connector, crtc},
        rustix::fs::{Dev, OFlags, major, minor},
    },
    utils::{Buffer as BufferCoord, DeviceFd, Physical, Rectangle, Size, Transform},
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{info, warn};
use weld_core::dmabuf::request_weld_device;

#[path = "smithay_drm_compositor_probe/vulkan.rs"]
mod vulkan;

use vulkan::{ForeignImageBarrier, foreign_image_barrier_command, renderable_scanout_formats};

const WGPU_SCANOUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;

#[derive(Debug, Parser)]
#[command(about = "Validate Smithay-owned GBM/KMS output with direct wgpu rendering")]
struct Arguments {
    /// Maximum probe duration before restoring the console.
    #[arg(long, default_value_t = 30)]
    seconds: u64,

    /// Ask libseat to switch to this VT after five seconds.
    #[arg(long)]
    switch_vt: Option<i32>,
}

#[derive(Debug)]
struct ProbeRenderError(String);

impl ProbeRenderError {
    fn message(message: impl fmt::Display) -> Self {
        Self(message.to_string())
    }
}

impl fmt::Display for ProbeRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ProbeRenderError {}

#[derive(Debug)]
struct ProbeTexture;

impl Texture for ProbeTexture {
    fn width(&self) -> u32 {
        0
    }

    fn height(&self) -> u32 {
        0
    }

    fn format(&self) -> Option<Fourcc> {
        None
    }
}

#[derive(Debug)]
struct ProbeFramebuffer {
    key: WeakDmabuf,
    view: wgpu::TextureView,
    image: vk::Image,
    size: Size<i32, Physical>,
    format: Fourcc,
}

impl Texture for ProbeFramebuffer {
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

struct ImportedScanout {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    image: vk::Image,
    used: bool,
}

impl fmt::Debug for ImportedScanout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportedScanout")
            .field("image", &self.image)
            .field("used", &self.used)
            .finish_non_exhaustive()
    }
}

struct ProbeRenderer {
    _instance: wgpu::Instance,
    _adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    raw_device: ash::Device,
    queue_family: u32,
    context: ContextId<ProbeTexture>,
    debug_flags: DebugFlags,
    formats: Vec<Format>,
    imports: HashMap<WeakDmabuf, ImportedScanout>,
}

impl fmt::Debug for ProbeRenderer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProbeRenderer")
            .field("queue_family", &self.queue_family)
            .field("context", &self.context)
            .field("debug_flags", &self.debug_flags)
            .field("formats", &self.formats)
            .field("imports", &self.imports)
            .finish_non_exhaustive()
    }
}

impl ProbeRenderer {
    fn new(device_id: Dev) -> Result<Self> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        descriptor.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(descriptor);
        let adapter = select_vulkan_adapter(&instance, device_id)?;
        let formats = renderable_scanout_formats(&adapter)?;
        if formats.is_empty() {
            bail!("selected Vulkan adapter exposes no explicit sRGB scanout modifier");
        }
        let (device, queue, _) = request_weld_device(&adapter, "Smithay DRM compositor probe")?;
        // SAFETY: these immutable Vulkan handles remain valid while the owning
        // wgpu device is retained by this renderer.
        let (raw_device, queue_family) = unsafe {
            let raw = device
                .as_hal::<wgpu::hal::api::Vulkan>()
                .context("probe device is not backed by Vulkan")?;
            (raw.raw_device().clone(), raw.queue_family_index())
        };
        info!(adapter = ?adapter.get_info(), formats = ?formats, "prepared probe renderer");
        Ok(Self {
            _instance: instance,
            _adapter: adapter,
            device,
            queue,
            raw_device,
            queue_family,
            context: ContextId::new(),
            debug_flags: DebugFlags::empty(),
            formats,
            imports: HashMap::new(),
        })
    }

    fn import(&mut self, dmabuf: &Dmabuf) -> Result<WeakDmabuf> {
        validate_scanout_target(dmabuf)?;
        self.prune_imports();
        let key = dmabuf.weak();
        if let Entry::Vacant(entry) = self.imports.entry(key.clone()) {
            entry.insert(import_scanout(&self.device, dmabuf)?);
        }
        Ok(key)
    }

    fn prune_imports(&mut self) {
        self.imports.retain(|dmabuf, _| !dmabuf.is_gone());
    }
}

impl RendererSuper for ProbeRenderer {
    type Error = ProbeRenderError;
    type TextureId = ProbeTexture;
    type Framebuffer<'buffer> = ProbeFramebuffer;
    type Frame<'frame, 'buffer>
        = ProbeFrame<'frame>
    where
        'buffer: 'frame,
        Self: 'frame;
}

impl Renderer for ProbeRenderer {
    fn context_id(&self) -> ContextId<Self::TextureId> {
        self.context.clone()
    }

    fn downscale_filter(&mut self, _filter: TextureFilter) -> Result<(), Self::Error> {
        Ok(())
    }

    fn upscale_filter(&mut self, _filter: TextureFilter) -> Result<(), Self::Error> {
        Ok(())
    }

    fn set_debug_flags(&mut self, flags: DebugFlags) {
        self.debug_flags = flags;
    }

    fn debug_flags(&self) -> DebugFlags {
        self.debug_flags
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
        if dst_transform != Transform::Normal {
            return Err(ProbeRenderError::message(format_args!(
                "probe renderer does not support output transform {dst_transform:?}"
            )));
        }
        if framebuffer.size != output_size {
            return Err(ProbeRenderError::message(format_args!(
                "Smithay requested {output_size:?} for framebuffer {:?}",
                framebuffer.size
            )));
        }
        let previously_used = self
            .imports
            .get(&framebuffer.key)
            .map(|imported| imported.used)
            .ok_or_else(|| ProbeRenderError::message("probe framebuffer import was evicted"))?;
        // SAFETY: the import cache retains the image through completion, and
        // this renderer owns the acquire-render-release sequence on one queue.
        let acquire = unsafe {
            foreign_image_barrier_command(
                &self.device,
                &self.raw_device,
                self.queue_family,
                framebuffer.image,
                previously_used,
                ForeignImageBarrier::Acquire,
            )
        }
        .map_err(ProbeRenderError::message)?;
        self.queue.submit([acquire]);
        self.imports
            .get_mut(&framebuffer.key)
            .ok_or_else(|| ProbeRenderError::message("probe framebuffer import was evicted"))?
            .used = true;
        Ok(ProbeFrame {
            device: &self.device,
            queue: &self.queue,
            raw_device: &self.raw_device,
            queue_family: self.queue_family,
            context: self.context.clone(),
            view: &framebuffer.view,
            image: framebuffer.image,
            output_size,
            color: Color32F::BLACK,
            released: false,
        })
    }

    fn wait(&mut self, sync: &SyncPoint) -> Result<(), Self::Error> {
        sync.wait().map_err(ProbeRenderError::message)
    }

    fn cleanup_texture_cache(&mut self) -> Result<(), Self::Error> {
        self.prune_imports();
        Ok(())
    }
}

impl Bind<Dmabuf> for ProbeRenderer {
    fn bind<'a>(&mut self, target: &'a mut Dmabuf) -> Result<Self::Framebuffer<'a>, Self::Error> {
        if target.format().modifier == Modifier::Invalid {
            return Err(ProbeRenderError::message(
                "Smithay selected an implicit modifier; the probe requires an explicit layout",
            ));
        }
        if !self.formats.contains(&target.format()) {
            return Err(ProbeRenderError::message(format_args!(
                "Smithay selected unsupported scanout format {:?}",
                target.format()
            )));
        }
        let key = self.import(target).map_err(ProbeRenderError::message)?;
        let imported = self
            .imports
            .get(&key)
            .ok_or_else(|| ProbeRenderError::message("probe framebuffer import was evicted"))?;
        Ok(ProbeFramebuffer {
            key,
            view: imported.view.clone(),
            image: imported.image,
            size: (target.size().w, target.size().h).into(),
            format: target.format().code,
        })
    }

    fn supported_formats(&self) -> Option<FormatSet> {
        Some(self.formats.iter().copied().collect())
    }
}

struct ProbeFrame<'a> {
    device: &'a wgpu::Device,
    queue: &'a wgpu::Queue,
    raw_device: &'a ash::Device,
    queue_family: u32,
    context: ContextId<ProbeTexture>,
    view: &'a wgpu::TextureView,
    image: vk::Image,
    output_size: Size<i32, Physical>,
    color: Color32F,
    released: bool,
}

impl ProbeFrame<'_> {
    fn release_command(&self) -> Result<wgpu::CommandBuffer, ProbeRenderError> {
        // SAFETY: the renderer cache retains this image, and the frame submits
        // release on the same queue that acquired and rendered it.
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
        .map_err(ProbeRenderError::message)
    }

    fn wait_for_submission(
        &self,
        submission: wgpu::SubmissionIndex,
    ) -> Result<(), ProbeRenderError> {
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: None,
            })
            .map(|_| ())
            .map_err(ProbeRenderError::message)
    }
}

impl Drop for ProbeFrame<'_> {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let completion = self
            .release_command()
            .map(|release| self.queue.submit([release]))
            .and_then(|submission| self.wait_for_submission(submission));
        if let Err(error) = completion {
            warn!(%error, "failed to release an aborted probe frame");
        }
        self.released = true;
    }
}

impl Frame for ProbeFrame<'_> {
    type Error = ProbeRenderError;
    type TextureId = ProbeTexture;

    fn context_id(&self) -> ContextId<Self::TextureId> {
        self.context.clone()
    }

    fn clear(
        &mut self,
        color: Color32F,
        at: &[Rectangle<i32, Physical>],
    ) -> Result<(), Self::Error> {
        if !at.is_empty() {
            self.color = color;
        }
        Ok(())
    }

    fn draw_solid(
        &mut self,
        _dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        color: Color32F,
    ) -> Result<(), Self::Error> {
        if !damage.is_empty() {
            self.color = color;
        }
        Ok(())
    }

    fn render_texture_from_to(
        &mut self,
        _texture: &Self::TextureId,
        _src: Rectangle<f64, BufferCoord>,
        _dst: Rectangle<i32, Physical>,
        _damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _src_transform: Transform,
        _alpha: f32,
    ) -> Result<(), Self::Error> {
        Err(ProbeRenderError::message(
            "texture rendering is outside this solid-color probe",
        ))
    }

    fn transformation(&self) -> Transform {
        Transform::Normal
    }

    fn output_size(&self) -> Size<i32, Physical> {
        self.output_size
    }

    fn wait(&mut self, sync: &SyncPoint) -> Result<(), Self::Error> {
        sync.wait().map_err(ProbeRenderError::message)
    }

    fn finish(mut self) -> Result<SyncPoint, Self::Error> {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Smithay DRM compositor probe draw"),
            });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Smithay DRM compositor probe solid pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: self.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(self.color.r()),
                            g: f64::from(self.color.g()),
                            b: f64::from(self.color.b()),
                            a: f64::from(self.color.a()),
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        let release = self.release_command()?;
        let submission = self.queue.submit([encoder.finish(), release]);
        self.released = true;
        self.wait_for_submission(submission)?;
        Ok(SyncPoint::signaled())
    }
}

#[derive(Debug)]
enum ProbeEvent {
    Exit,
    Session(SessionEvent),
    Drm {
        event: DrmEvent,
        metadata: Option<DrmEventMetadata>,
    },
}

#[derive(Default)]
struct ProbeEvents(VecDeque<ProbeEvent>);

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    tracing_subscriber::fmt()
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init()
        .map_err(|error| anyhow!("failed to initialize tracing: {error}"))?;

    let mut event_loop =
        EventLoop::<ProbeEvents>::try_new().context("failed to create probe event loop")?;
    let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM])
        .map_err(|error| anyhow!("failed to initialize signal handling: {error}"))?;
    event_loop
        .handle()
        .insert_source(signals, |_, _, events| {
            events.0.push_back(ProbeEvent::Exit);
        })
        .context("failed to register signal handling")?;

    let (mut session, session_notifier) =
        LibSeatSession::new().context("failed to acquire a libseat session")?;
    let seat_name = session.seat();
    let udev = UdevBackend::new(&seat_name).context("failed to initialize udev discovery")?;
    let (device_id, device_path) = select_device(&mut session, udev.device_list())?;
    let fd = session
        .open(
            &device_path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .with_context(|| format!("failed to open DRM device {}", device_path.display()))?;
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(fd));
    let (connector, output_crtc, mode) = select_output(&drm_fd)?;
    let connector_name = format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    );

    let (drm_device, drm_notifier) =
        DrmDevice::new(drm_fd.clone(), true).context("failed to initialize Smithay DRM device")?;
    let gbm = GbmDevice::new(drm_fd.clone()).context("failed to create GBM device")?;
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let exporter = GbmFramebufferExporter::new(gbm, NodeFilter::None);
    let mut renderer = ProbeRenderer::new(device_id)?;
    let renderer_formats = renderer.formats.clone();
    let mut output_manager = DrmOutputManager::new(
        drm_device,
        allocator,
        exporter,
        None::<GbmDevice<DrmDeviceFd>>,
        [Fourcc::Argb8888],
        renderer_formats,
    );
    let mode_size = mode.size();
    let output_mode = OutputModeSource::Static {
        size: (i32::from(mode_size.0), i32::from(mode_size.1)).into(),
        scale: 1.0.into(),
        transform: Transform::Normal,
    };
    let mut output = output_manager
        .lock()
        .initialize_output(
            output_crtc,
            mode,
            &[connector.handle()],
            output_mode,
            None,
            &mut renderer,
            &DrmOutputRenderElements::<ProbeRenderer, SolidColorRenderElement>::default(),
        )
        .context("Smithay failed to initialize the probe output")?;
    info!(
        connector = %connector_name,
        crtc = ?output_crtc,
        size = ?mode_size,
        fourcc = ?output.format(),
        formats = ?renderer.formats,
        "Smithay owns the probe output and explicit-modifier swapchain"
    );

    event_loop
        .handle()
        .insert_source(session_notifier, |event, _, events| {
            events.0.push_back(ProbeEvent::Session(event));
        })
        .map_err(|_| anyhow!("failed to register libseat notifications"))?;
    event_loop
        .handle()
        .insert_source(drm_notifier, |event, metadata, events| {
            events.0.push_back(ProbeEvent::Drm {
                event,
                metadata: *metadata,
            });
        })
        .map_err(|_| anyhow!("failed to register DRM page-flip notifications"))?;

    let mut solid = SolidColorBuffer::new(
        (i32::from(mode_size.0), i32::from(mode_size.1)),
        [0.04, 0.12, 0.3, 1.0],
    );
    let mut next_frame = 1_u64;
    queue_probe_frame(&mut output, &mut renderer, &mut solid, next_frame)?;
    let started_at = Instant::now();
    let deadline = started_at + Duration::from_secs(arguments.seconds);
    let switch_at = arguments
        .switch_vt
        .map(|_| started_at + Duration::from_secs(5));
    let mut switch_requested = false;
    let mut active = session.is_active();
    let mut events = ProbeEvents::default();
    let mut retired_frames = 0_u64;

    info!(
        seconds = arguments.seconds,
        switch_vt = arguments.switch_vt,
        "probe started; press Ctrl+C to exit"
    );
    'running: loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if !switch_requested
            && switch_at.is_some_and(|switch_at| now >= switch_at)
            && let Some(vt) = arguments.switch_vt
        {
            info!(vt, "requesting VT switch through libseat");
            // Stop queueing before libseat can revoke DRM access. A final
            // vblank may arrive before calloop delivers PauseSession.
            active = false;
            session
                .change_vt(vt)
                .with_context(|| format!("failed to switch to VT {vt}"))?;
            switch_requested = true;
        }
        event_loop
            .dispatch(
                Some(
                    Duration::from_millis(100)
                        .min(deadline.saturating_duration_since(Instant::now())),
                ),
                &mut events,
            )
            .context("probe event dispatch failed")?;
        while let Some(event) = events.0.pop_front() {
            match event {
                ProbeEvent::Exit => break 'running,
                ProbeEvent::Session(SessionEvent::PauseSession) => {
                    active = false;
                    output_manager.pause();
                    info!("session paused; Smithay output presentation stopped");
                }
                ProbeEvent::Session(SessionEvent::ActivateSession) => {
                    output_manager
                        .lock()
                        .activate(true)
                        .context("failed to reactivate Smithay DRM output manager")?;
                    active = true;
                    next_frame = next_frame.saturating_add(1);
                    queue_probe_frame(&mut output, &mut renderer, &mut solid, next_frame)?;
                    info!("session activated; Smithay queued a fresh probe frame");
                }
                ProbeEvent::Drm {
                    event: DrmEvent::VBlank(crtc),
                    metadata,
                } if crtc == output_crtc => {
                    let submitted = output
                        .frame_submitted()
                        .context("Smithay failed to retire the probe frame")?;
                    if let Some(frame) = submitted {
                        retired_frames = retired_frames.saturating_add(1);
                        info!(
                            frame,
                            sequence = metadata.map(|metadata| metadata.sequence),
                            time = ?metadata.map(|metadata| metadata.time),
                            "retired Smithay output frame"
                        );
                    }
                    if active {
                        next_frame = next_frame.saturating_add(1);
                        queue_probe_frame(&mut output, &mut renderer, &mut solid, next_frame)?;
                    }
                }
                ProbeEvent::Drm {
                    event: DrmEvent::VBlank(_),
                    ..
                } => {}
                ProbeEvent::Drm {
                    event: DrmEvent::Error(error),
                    ..
                } => return Err(error).context("Smithay DRM notifier failed"),
            }
        }
    }

    output_manager.pause();
    info!(retired_frames, "Smithay DRM compositor probe finished");
    Ok(())
}

fn queue_probe_frame<A, F, G>(
    output: &mut smithay::backend::drm::output::DrmOutput<A, F, u64, G>,
    renderer: &mut ProbeRenderer,
    solid: &mut SolidColorBuffer,
    frame: u64,
) -> Result<()>
where
    A: smithay::backend::allocator::Allocator + Clone + fmt::Debug,
    A::Buffer: smithay::backend::allocator::dmabuf::AsDmabuf,
    A::Error: Send + Sync + 'static,
    <A::Buffer as smithay::backend::allocator::dmabuf::AsDmabuf>::Error: Send + Sync + 'static,
    F: smithay::backend::drm::exporter::ExportFramebuffer<A::Buffer> + Clone,
    F::Framebuffer: fmt::Debug + Send + Sync + 'static,
    F::Error: Send + Sync + 'static,
    G: std::os::fd::AsFd + Clone + 'static,
{
    let phase = (frame % 240) as f32 / 240.0;
    solid.set_color([0.04 + phase * 0.2, 0.12, 0.3 + (1.0 - phase) * 0.2, 1.0]);
    let element = SolidColorRenderElement::from_buffer(solid, (0, 0), 1.0, 1.0, Kind::Unspecified);
    let elements = [element];
    let result = output
        .render_frame(renderer, &elements, Color32F::BLACK, FrameFlags::empty())
        .context("Smithay failed to render the probe frame")?;
    if result.needs_sync()
        && let PrimaryPlaneElement::Swapchain(primary) = &result.primary_element
    {
        primary
            .sync
            .wait()
            .context("probe render synchronization was interrupted")?;
    }
    drop(result);
    output
        .queue_frame(frame)
        .context("Smithay failed to queue the probe frame")?;
    Ok(())
}

fn select_device<'a>(
    session: &mut LibSeatSession,
    devices: impl Iterator<Item = (Dev, &'a std::path::Path)>,
) -> Result<(Dev, PathBuf)> {
    let primary = primary_gpu(session.seat())?.context("no DRM GPU was found for the seat")?;
    let devices = devices
        .map(|(device_id, path)| (device_id, path.to_path_buf()))
        .collect::<Vec<_>>();
    devices
        .iter()
        .find(|(_, path)| *path == primary)
        .cloned()
        .or_else(|| devices.first().cloned())
        .context("udev reported no DRM devices for the seat")
}

fn select_output(drm: &DrmDeviceFd) -> Result<(connector::Info, crtc::Handle, Mode)> {
    let mut scanner: DrmScanner = DrmScanner::new();
    let (connector, output_crtc) = scanner
        .scan_connectors(drm)?
        .into_iter()
        .find_map(|event| match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(output_crtc),
            } if !connector.modes().is_empty() => Some((connector, output_crtc)),
            _ => None,
        })
        .context("no connected DRM connector with a usable CRTC and mode")?;
    let mode = connector
        .modes()
        .iter()
        .copied()
        .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector.modes().first().copied())
        .context("DRM connector has no modes")?;
    Ok((connector, output_crtc, mode))
}

fn select_vulkan_adapter(instance: &wgpu::Instance, device_id: Dev) -> Result<wgpu::Adapter> {
    pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN))
        .into_iter()
        .find(|adapter| adapter_matches_device(adapter, device_id))
        .context("no Vulkan adapter matches the selected DRM device")
}

fn adapter_matches_device(adapter: &wgpu::Adapter, device_id: Dev) -> bool {
    // SAFETY: this guard only queries immutable Vulkan adapter properties.
    let Some(adapter) = (unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }) else {
        return false;
    };
    let mut drm_properties = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm_properties);
    // SAFETY: the handles and output chain belong to this live adapter.
    unsafe {
        adapter
            .shared_instance()
            .raw_instance()
            .get_physical_device_properties2(adapter.raw_physical_device(), &mut properties);
    }
    let selected_major = i64::from(major(device_id));
    let selected_minor = i64::from(minor(device_id));
    (drm_properties.has_primary != 0
        && drm_properties.primary_major == selected_major
        && drm_properties.primary_minor == selected_minor)
        || (drm_properties.has_render != 0
            && drm_properties.render_major == selected_major
            && drm_properties.render_minor == selected_minor)
}

fn validate_scanout_target(dmabuf: &Dmabuf) -> Result<()> {
    if dmabuf.num_planes() != 1 {
        bail!("probe requires exactly one DMA-BUF plane");
    }
    if dmabuf.format().modifier == Modifier::Invalid {
        bail!("probe refuses implicit DMA-BUF modifiers");
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
        label: Some("Smithay DRM compositor probe import"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: WGPU_SCANOUT_FORMAT,
        usage: wgpu::TextureUses::COLOR_TARGET,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: Vec::new(),
    };
    // SAFETY: capability discovery and Smithay selected this exact explicit
    // single-plane modifier for scanout and Vulkan color-attachment use.
    let hal_texture = unsafe {
        let raw = device
            .as_hal::<wgpu::hal::api::Vulkan>()
            .context("probe device is not backed by Vulkan")?;
        raw.texture_from_dmabuf_fd(
            fd,
            &hal_descriptor,
            dmabuf.format().modifier.into(),
            stride,
            offset,
        )?
    };
    let descriptor = wgpu::TextureDescriptor {
        label: Some("Smithay DRM compositor probe import"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: WGPU_SCANOUT_FORMAT,
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
    // SAFETY: the texture is retained in the import cache while the raw image
    // handle is used for ownership barriers.
    let image = unsafe {
        texture
            .as_hal::<wgpu::hal::api::Vulkan>()
            .context("imported scanout texture is not backed by Vulkan")?
            .raw_handle()
    };
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    info!(format = ?dmabuf.format(), size = ?dmabuf.size(), "imported Smithay-owned scanout buffer");
    Ok(ImportedScanout {
        _texture: texture,
        view,
        image,
        used: false,
    })
}
