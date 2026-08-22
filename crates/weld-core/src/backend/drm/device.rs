//! Session, GPU, and Smithay output-manager bootstrap.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use calloop::channel::{self, Channel};
use smithay::{
    backend::{
        allocator::{
            Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmDeviceNotifier,
            exporter::gbm::{GbmFramebufferExporter, NodeFilter},
            output::{DrmOutput, DrmOutputManager},
        },
        session::{
            Session,
            libseat::{LibSeatSession, LibSeatSessionNotifier},
        },
        udev::{UdevBackend, primary_gpu},
    },
    reexports::rustix::fs::{Dev, OFlags},
    utils::DeviceFd,
};

use crate::{
    dmabuf::{DmabufCapabilities, DmabufReleaseId, DmabufSourceCache, request_weld_device},
    host::{RenderContext, RunOptions},
};

use super::{
    output::{SelectedOutput, select_output},
    renderer::DrmRenderState,
    vulkan::{adapter_matches_device, renderable_scanout_formats},
};

pub(super) type OutputAllocator = GbmAllocator<DrmDeviceFd>;
pub(super) type OutputExporter = GbmFramebufferExporter<DrmDeviceFd>;
pub(super) type OutputManager =
    DrmOutputManager<OutputAllocator, OutputExporter, SubmittedFrame, DrmDeviceFd>;
pub(super) type PhysicalOutput =
    DrmOutput<OutputAllocator, OutputExporter, SubmittedFrame, DrmDeviceFd>;

#[derive(Clone, Copy, Debug)]
pub(super) struct SubmittedFrame {
    pub(super) presentation_id: Option<u64>,
}

pub(super) struct DrmBootstrap {
    pub(super) runtime: DrmRuntimeBootstrap,
    pub(super) render_context: RenderContext,
}

pub(super) struct DrmRuntimeBootstrap {
    pub(super) session: LibSeatSession,
    pub(super) session_notifier: LibSeatSessionNotifier,
    pub(super) drm_notifier: DrmDeviceNotifier,
    pub(super) output_manager: OutputManager,
    pub(super) render_state: DrmRenderState,
    pub(super) selected_output: SelectedOutput,
    pub(super) dmabuf_capabilities: Option<DmabufCapabilities>,
    pub(super) dmabuf_sources: DmabufSourceCache,
    pub(super) dmabuf_release_source: Channel<DmabufReleaseId>,
}

pub(super) fn prepare(options: &RunOptions) -> Result<DrmBootstrap> {
    let (mut session, session_notifier) =
        LibSeatSession::new().context("failed to acquire a libseat session")?;
    let udev = UdevBackend::new(session.seat()).context("failed to initialize udev discovery")?;
    let (device_id, device_path) = select_device(&mut session, udev.device_list())?;
    let fd = session
        .open(
            &device_path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .with_context(|| format!("failed to open DRM device {}", device_path.display()))?;
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(fd));
    let selected_output = select_output(&drm_fd, options.output_scale)?;

    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
    descriptor.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(descriptor);
    let adapter = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN))
        .into_iter()
        .find(|adapter| adapter_matches_device(adapter, device_id))
        .context("no Vulkan adapter matches the selected DRM device")?;
    let scanout_formats = renderable_scanout_formats(&adapter)?;
    if scanout_formats.is_empty() {
        bail!("selected Vulkan adapter exposes no explicit sRGB scanout modifier");
    }
    let (device, queue, dmabuf_capabilities) = request_weld_device(&adapter, "Weld DRM device")?;
    let dmabuf_sources = DmabufSourceCache::new(&device);
    let render_state = DrmRenderState::new(device.clone(), queue.clone(), scanout_formats.clone())?;

    let (drm_device, drm_notifier) =
        DrmDevice::new(drm_fd.clone(), true).context("failed to initialize Smithay DRM device")?;
    let gbm = GbmDevice::new(drm_fd).context("failed to create GBM device")?;
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let exporter = GbmFramebufferExporter::new(gbm.clone(), NodeFilter::None);
    let output_manager = DrmOutputManager::new(
        drm_device,
        allocator,
        exporter,
        Some(gbm),
        [Fourcc::Argb8888],
        scanout_formats,
    );
    let (release_sender, dmabuf_release_source) = channel::channel();
    let render_context = RenderContext {
        instance,
        adapter,
        device,
        queue,
        dmabuf: crate::dmabuf::DmabufContext::new(release_sender, dmabuf_sources.clone()),
        output_heads: vec![selected_output.head.clone()],
        outputs: vec![selected_output.configuration],
        composition_format: wgpu::TextureFormat::Bgra8UnormSrgb,
    };
    Ok(DrmBootstrap {
        render_context,
        runtime: DrmRuntimeBootstrap {
            session,
            session_notifier,
            drm_notifier,
            output_manager,
            render_state,
            selected_output,
            dmabuf_capabilities,
            dmabuf_sources,
            dmabuf_release_source,
        },
    })
}

fn select_device<'a>(
    session: &mut LibSeatSession,
    devices: impl Iterator<Item = (Dev, &'a Path)>,
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
