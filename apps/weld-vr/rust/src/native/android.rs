use super::{Geometry, Progress};
use anyhow::{Context, Result};
use std::{ffi::c_void, os::fd::OwnedFd, ptr::NonNull};
use weld_media::DecoderConfig;
use weld_media_android::{AndroidDecoder, AndroidImage, AndroidImageTarget, DecodeProgress};

pub const TEXTURE_TARGET: u32 = 0x8d65; // GL_TEXTURE_EXTERNAL_OES
// Fixture holders: current/spare (2), retirements (2), pending (1), latest (1),
// acquisition (1). Live receive reserves one of seven per-stream frame credits
// and one of 32 session credits BEFORE submission, through GPU release.
// Thus even pool completions/old-generation outputs cannot exhaust this reader.
const MAX_ACQUIRED_IMAGES: i32 = 8;
#[derive(Clone)]
pub struct Target;
impl Target {
    pub fn query(_context: NonNull<c_void>) -> Result<Self> {
        Ok(Self)
    }
}
pub struct Decoder(AndroidDecoder);
impl Decoder {
    pub fn new(config: &DecoderConfig, target: Target) -> Result<Self> {
        Self::new_with_low_latency(config, target, false)
    }
    pub fn new_with_low_latency(
        config: &DecoderConfig,
        _target: Target,
        low_latency: bool,
    ) -> Result<Self> {
        let (width, height) = config.extent();
        let decoder = AndroidDecoder::new_with_low_latency(
            config,
            AndroidImageTarget::new(width, height, MAX_ACQUIRED_IMAGES)?,
            low_latency,
        )?;
        tracing::info!(target: "weld_vr_diag", requested_low_latency = low_latency,
            width, height, "opened Android decoder; low-latency support not verified");
        Ok(Self(decoder))
    }
    pub fn try_send(&mut self, bytes: &[u8], timestamp: u64) -> Result<bool> {
        self.0.try_send(bytes, timestamp)
    }
    pub fn try_finish(&mut self) -> Result<bool> {
        self.0.try_finish()
    }
    pub fn receive(&mut self, cancelled: impl Fn() -> bool) -> Result<Progress> {
        Ok(match self.0.receive(cancelled)? {
            DecodeProgress::Pending => Progress::Pending,
            DecodeProgress::End => Progress::End,
            DecodeProgress::Image(image) => Progress::Image(Image(image)),
        })
    }
}
pub struct Image(AndroidImage);
impl Image {
    pub fn timestamp_micros(&self) -> u64 {
        self.0.info().timestamp_micros
    }
    pub fn geometry(&self) -> Geometry {
        let info = self.0.info();
        Geometry {
            width: info.width,
            height: info.height,
            crop: info.crop,
        }
    }
    /// Caller must retain this lease through every GPU read and use its current
    /// EGL context. The returned image is owned by the render adapter.
    pub unsafe fn import(&self, context: NonNull<c_void>) -> Result<NonNull<c_void>> {
        // SAFETY: caller retains this image through the imported GPU storage lifetime.
        let buffer = unsafe { self.0.hardware_buffer_ptr()? };
        // SAFETY: borrowed buffer is live; helper verifies the saved current context.
        NonNull::new(unsafe { weld_egl_import_android(context.as_ptr(), buffer) })
            .context("Android EGLImage import failed")
    }
    pub fn release(self, fence: OwnedFd) {
        self.0.release_after(fence);
    }
}
unsafe extern "C" {
    fn weld_egl_import_android(context: *mut c_void, buffer: *mut c_void) -> *mut c_void;
}
