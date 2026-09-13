use super::{Geometry, Progress};
use anyhow::{Context, Result, ensure};
use std::{
    ffi::c_void,
    os::fd::{AsRawFd, OwnedFd},
    path::PathBuf,
    ptr::NonNull,
};
use weld_media::DecoderConfig;
use weld_media_vaapi::{FfmpegDecoder, FfmpegVaapiDevice, VaapiDevice, VaapiDmabuf, VppConverter};

pub const TEXTURE_TARGET: u32 = 0x0de1; // GL_TEXTURE_2D
#[derive(Clone)]
pub struct Target {
    modifiers: Vec<u64>,
}
impl Target {
    pub fn query(context: NonNull<c_void>) -> Result<Self> {
        let mut modifiers = [0u64; 64];
        // SAFETY: context is current on the render thread; bounded writable array.
        let count = unsafe {
            weld_egl_xrgb_modifiers(
                context.as_ptr(),
                modifiers.as_mut_ptr(),
                modifiers.len() as i32,
            )
        };
        ensure!(
            count >= 0 && count as usize <= modifiers.len(),
            "invalid EGL modifier query"
        );
        let modifiers = if count == 0 {
            vec![0]
        } else {
            modifiers[..count as usize].to_vec()
        };
        Ok(Self { modifiers })
    }
}
// Every native context is constructed and dropped on the decoder worker.
pub struct Decoder {
    pending: Option<Image>,
    decoder: FfmpegDecoder,
    vpp: VppConverter,
    _device: FfmpegVaapiDevice,
    target: Target,
    extent: (u32, u32),
    ended: bool,
}
impl Decoder {
    pub fn new(config: &DecoderConfig, target: Target) -> Result<Self> {
        ensure!(
            config.extra().is_empty(),
            "Linux fixture expects in-band codec headers"
        );
        let path = std::env::var_os("WELD_VR_RENDER_NODE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/dev/dri/renderD128"));
        let device = FfmpegVaapiDevice::open(&path)?;
        let decoder = FfmpegDecoder::new(config.codec(), &device, 1)?;
        let vpp = VaapiDevice::open(&path)?.vpp_converter()?;
        Ok(Self {
            decoder,
            vpp,
            _device: device,
            target,
            extent: config.extent(),
            pending: None,
            ended: false,
        })
    }
    pub fn try_send(&mut self, bytes: &[u8], timestamp: u64) -> Result<bool> {
        ensure!(!self.ended, "decoder already ended");
        if self.pending.is_some() {
            return Ok(false);
        }
        let frame = self.decoder.submit(bytes, timestamp)?;
        let decoded = frame.finish(
            self.extent.0,
            self.extent.1,
            &self.target.modifiers,
            &self.vpp,
        )?;
        self.pending = Some(Image {
            buffer: decoded.dmabuf,
            timestamp,
        });
        Ok(true)
    }
    pub fn try_finish(&mut self) -> Result<bool> {
        self.ended = true;
        Ok(true)
    }
    pub fn receive(&mut self, _cancelled: impl Fn() -> bool) -> Result<Progress> {
        Ok(if let Some(image) = self.pending.take() {
            Progress::Image(image)
        } else if self.ended {
            Progress::End
        } else {
            Progress::Pending
        })
    }
}
/// VPP creates an independent allocation and completes its writes before this
/// export. Any future output pooling must wait for this lease to be released.
pub struct Image {
    buffer: VaapiDmabuf,
    timestamp: u64,
}
impl Image {
    pub fn timestamp_micros(&self) -> u64 {
        self.timestamp
    }
    pub fn geometry(&self) -> Geometry {
        Geometry {
            width: self.buffer.width,
            height: self.buffer.height,
            crop: [0, 0, self.buffer.width, self.buffer.height],
        }
    }
    /// Caller must retain this lease through GPU reads and supply its current
    /// EGL context. The returned image is owned by the render adapter.
    pub unsafe fn import(&self, context: NonNull<c_void>) -> Result<NonNull<c_void>> {
        ensure!(
            self.buffer.fourcc == u32::from_le_bytes(*b"XR24") && self.buffer.planes.len() == 1,
            "expected one-plane XRGB output"
        );
        let plane = &self.buffer.planes[0];
        let object = self
            .buffer
            .objects
            .get(usize::from(plane.object_index))
            .context("missing DMA-BUF object")?;
        let width = i32::try_from(self.buffer.width)?;
        let height = i32::try_from(self.buffer.height)?;
        let offset = i32::try_from(plane.offset)?;
        let stride = i32::try_from(plane.stride)?;
        // SAFETY: descriptor remains owned by this lease; helper imports it without
        // taking fd ownership and verifies the EGL context and modifier support.
        NonNull::new(unsafe {
            weld_egl_import_dmabuf(
                context.as_ptr(),
                width,
                height,
                object.file_descriptor.as_raw_fd(),
                offset,
                stride,
                object.modifier,
            )
        })
        .context("Linux EGLImage import failed")
    }
    pub fn release(self, fence: OwnedFd) {
        drop(fence);
        drop(self);
    }
}
unsafe extern "C" {
    fn weld_egl_xrgb_modifiers(context: *mut c_void, modifiers: *mut u64, capacity: i32) -> i32;
    fn weld_egl_import_dmabuf(
        context: *mut c_void,
        width: i32,
        height: i32,
        fd: i32,
        offset: i32,
        stride: i32,
        modifier: u64,
    ) -> *mut c_void;
}
