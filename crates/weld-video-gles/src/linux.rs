//! VA-API decode workers and EGL DMA-BUF texture import on Blitz's GLES device.
use anyhow::{Context, Result, ensure};
use std::{num::NonZeroU32, os::fd::AsRawFd, path::PathBuf};
use weld_hoist_encoded::{
    DecodeBackend, DecodeCompletion, DecodeRequest, DecodedFrame, SubmitError,
};
use weld_media::{MediaStreamId, StreamGeneration, VideoCodec, WorkerSubmitError};
pub use weld_media_vaapi::VaapiDmabuf as Image;
use weld_media_vaapi::{VaapiDecodeRequest, VaapiDecodeWorker};
pub fn converter(handle: &wgpu_context::DeviceHandle) -> Result<Converter> {
    Converter::new(handle)
}
pub fn extent(image: &Image) -> [u32; 2] {
    [image.width, image.height]
}

struct Backend {
    worker: VaapiDecodeWorker,
    codec: VideoCodec,
}
pub fn backend(codec: VideoCodec) -> Result<Box<dyn DecodeBackend<Output = Image>>> {
    let owner = std::thread::current();
    let node = std::env::var_os("WELD_PROBE_RENDER_NODE")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/dev/dri/renderD128".into());
    Ok(Box::new(Backend {
        worker: VaapiDecodeWorker::spawn(node, move || owner.unpark())?,
        codec,
    }))
}
impl DecodeBackend for Backend {
    type Output = Image;
    fn try_submit(&mut self, request: DecodeRequest) -> Result<(), SubmitError<DecodeRequest>> {
        if request.access_unit.codec != self.codec {
            return Err(SubmitError::Rejected(anyhow::anyhow!("unexpected codec")));
        }
        self.worker
            .try_decode(VaapiDecodeRequest {
                token: request.token,
                access_unit: request.access_unit,
                visible_width: request.visible_width,
                visible_height: request.visible_height,
                xrgb_modifiers: vec![0],
            })
            .map_err(|error| {
                let (stopped, request) = match error {
                    WorkerSubmitError::Rejected(error) => return SubmitError::Rejected(error),
                    WorkerSubmitError::Busy(request) => (false, request),
                    WorkerSubmitError::Stopped(request) => (true, request),
                };
                let request = DecodeRequest {
                    token: request.token,
                    access_unit: request.access_unit,
                    visible_width: request.visible_width,
                    visible_height: request.visible_height,
                };
                if stopped {
                    SubmitError::Stopped(request)
                } else {
                    SubmitError::Busy(request)
                }
            })
    }
    fn drain(&mut self) -> (Vec<DecodeCompletion<Image>>, Option<anyhow::Error>) {
        let (items, error) = self.worker.drain();
        (
            items
                .into_iter()
                .map(|item| DecodeCompletion {
                    token: item.token,
                    timing: item.timing,
                    result: item.result.map(|frames| {
                        frames
                            .into_iter()
                            .map(|frame| DecodedFrame {
                                frame: frame.frame,
                                buffer: frame.dmabuf,
                            })
                            .collect()
                    }),
                })
                .collect(),
            error,
        )
    }
    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()> {
        self.worker.retire(stream, generation)
    }
}

pub struct Converter {
    device: wgpu::Device,
    queue: wgpu::Queue,
    current: Option<Image>,
}
impl Converter {
    pub fn new(handle: &wgpu_context::DeviceHandle) -> Result<Self> {
        // SAFETY: read-only backend check on the retained live device.
        ensure!(
            unsafe { handle.device.as_hal::<wgpu::hal::api::Gles>() }.is_some(),
            "Linux probe requires WGPU_BACKEND=gl"
        );
        Ok(Self {
            device: handle.device.clone(),
            queue: handle.queue.clone(),
            current: None,
        })
    }
    pub fn convert(&mut self, image: Image) -> Result<wgpu::Texture> {
        ensure!(
            image.fourcc == u32::from_le_bytes(*b"XR24") && image.planes.len() == 1,
            "expected one-plane XRGB DMA-BUF"
        );
        let plane = &image.planes[0];
        let object = image
            .objects
            .get(usize::from(plane.object_index))
            .context("missing DMA-BUF object")?;
        let size = wgpu::Extent3d {
            width: image.width,
            height: image.height,
            depth_or_array_layers: 1,
        };
        let raw = {
            // SAFETY: all EGL operations use the renderer's locked current context.
            let hal =
                unsafe { self.device.as_hal::<wgpu::hal::api::Gles>() }.context("GLES device")?;
            let _context = hal.context().lock();
            // SAFETY: live owned DMA-BUF, validated one-plane descriptor, current context.
            let name = NonZeroU32::new(unsafe {
                weld_probe_import(
                    i32::try_from(image.width)?,
                    i32::try_from(image.height)?,
                    object.file_descriptor.as_raw_fd(),
                    i32::try_from(plane.offset)?,
                    i32::try_from(plane.stride)?,
                    object.modifier,
                )
            })
            .context("EGL DMA-BUF import failed")?;
            let descriptor = wgpu::hal::TextureDescriptor {
                label: Some("probe decoded DMA-BUF"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUses::RESOURCE,
                memory_flags: wgpu::hal::MemoryFlags::empty(),
                view_formats: vec![],
            };
            // SAFETY: import creates a matching initialized texture; HAL owns GL deletion.
            unsafe { hal.texture_from_raw(name, &descriptor, None) }
        };
        let descriptor = wgpu::TextureDescriptor {
            label: Some("probe decoded DMA-BUF"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        // SAFETY: descriptor and device match the imported initialized HAL texture.
        let texture = unsafe {
            self.device.create_texture_from_hal::<wgpu::hal::api::Gles>(
                raw,
                &descriptor,
                wgpu::TextureUses::RESOURCE,
            )
        };
        if let Some(previous) = self.current.replace(image) {
            // Previous paint has been submitted before this paint begins.
            self.queue.on_submitted_work_done(move || drop(previous));
        }
        Ok(texture)
    }
}
impl Drop for Converter {
    fn drop(&mut self) {
        if let Some(image) = self.current.take() {
            self.queue.on_submitted_work_done(move || drop(image));
        }
    }
}
unsafe extern "C" {
    fn weld_probe_import(
        width: i32,
        height: i32,
        fd: i32,
        offset: i32,
        stride: i32,
        modifier: u64,
    ) -> u32;
}
