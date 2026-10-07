//! The phone's GLES converter, publishing an ordinary texture to Blitz's device.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::c_void,
    num::NonZeroU32,
    os::fd::{FromRawFd, OwnedFd},
    ptr::NonNull,
};
use weld_media_android::AndroidImage;
use wgpu::{Texture, TextureUses};

pub struct Converter {
    native: NonNull<c_void>,
    device: wgpu::Device,
    output: Option<(NonZeroU32, Texture)>,
    fence_reported: bool,
}
impl Converter {
    pub fn new(device: &wgpu::Device) -> Result<Self> {
        // SAFETY: borrow the live device and serialize all EGL access through its lock.
        let hal = unsafe { device.as_hal::<wgpu::hal::api::Gles>() }
            .context("probe requires the GLES backend")?;
        let _context = hal.context().lock();
        // SAFETY: the converter is created on the locked current EGL context.
        let native = NonNull::new(unsafe { weld_probe_video_open() })
            .context("EGL video import unavailable")?;
        Ok(Self {
            native,
            device: device.clone(),
            output: None,
            fence_reported: false,
        })
    }

    pub fn convert(&mut self, frame: AndroidImage) -> Result<Texture> {
        let info = frame.info();
        let width = info.crop[2]
            .checked_sub(info.crop[0])
            .context("crop width")?;
        let height = info.crop[3]
            .checked_sub(info.crop[1])
            .context("crop height")?;
        ensure!(
            width > 0 && height > 0 && info.crop[2] <= info.width && info.crop[3] <= info.height,
            "invalid crop"
        );
        ensure!(
            width <= self.device.limits().max_texture_dimension_2d
                && height <= self.device.limits().max_texture_dimension_2d,
            "decoded extent exceeds renderer limit"
        );
        let descriptor = wgpu::TextureDescriptor {
            label: Some("Blitz Android decoded video"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        if self
            .output
            .as_ref()
            .is_none_or(|(_, texture)| texture.size() != descriptor.size)
        {
            let (name, raw) = {
                // SAFETY: same retained device as the converter.
                let hal = unsafe { self.device.as_hal::<wgpu::hal::api::Gles>() }
                    .context("GLES device")?;
                let _context = hal.context().lock();
                // SAFETY: positive dimensions; locked current context.
                let name = NonZeroU32::new(unsafe {
                    weld_probe_video_texture(i32::try_from(width)?, i32::try_from(height)?)
                })
                .context("texture allocation")?;
                let desc = wgpu::hal::TextureDescriptor {
                    label: descriptor.label,
                    size: descriptor.size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: descriptor.dimension,
                    format: descriptor.format,
                    usage: TextureUses::RESOURCE,
                    memory_flags: wgpu::hal::MemoryFlags::empty(),
                    view_formats: vec![],
                };
                // SAFETY: C allocated this exact unorm texture. HAL takes deletion ownership.
                (name, unsafe { hal.texture_from_raw(name, &desc, None) })
            };
            // SAFETY: matching device and allocation; convert initializes pixels before publication.
            let texture = unsafe {
                self.device.create_texture_from_hal::<wgpu::hal::api::Gles>(
                    raw,
                    &descriptor,
                    TextureUses::RESOURCE,
                )
            };
            self.output = Some((name, texture));
        }
        let (name, texture) = self.output.as_ref().context("video output")?;
        let crop = [
            info.crop[0] as f32 / info.width as f32,
            info.crop[1] as f32 / info.height as f32,
            width as f32 / info.width as f32,
            height as f32 / info.height as f32,
        ];
        let fence = {
            // SAFETY: every raw GL access uses the same locked context as WGPU.
            let hal =
                unsafe { self.device.as_hal::<wgpu::hal::api::Gles>() }.context("GLES device")?;
            let _context = hal.context().lock();
            // SAFETY: image lease remains live until its completion fence is transferred below.
            unsafe {
                weld_mobile_video_draw(
                    self.native.as_ptr(),
                    frame.hardware_buffer_ptr()?,
                    name.get(),
                    i32::try_from(width)?,
                    i32::try_from(height)?,
                    crop.as_ptr(),
                )
            }
        };
        if fence >= 0 {
            if !self.fence_reported {
                log::info!("GPU import uses native fence release");
                self.fence_reported = true;
            }
            // SAFETY: the converter returns one newly owned fence descriptor.
            frame.release_after(unsafe { OwnedFd::from_raw_fd(fence) });
        } else {
            ensure!(fence == -2, "video conversion failed");
            if !self.fence_reported {
                log::warn!("GPU import uses synchronous release fallback");
                self.fence_reported = true;
            }
            // The C fallback has completed the source read synchronously.
            drop(frame);
        }
        Ok(texture.clone())
    }
}
impl Drop for Converter {
    fn drop(&mut self) {
        // SAFETY: retained device outlives all converter operations and destruction.
        if let Some(hal) = unsafe { self.device.as_hal::<wgpu::hal::api::Gles>() } {
            let _context = hal.context().lock();
            // SAFETY: unique converter, destroyed once with its creating context current.
            unsafe { weld_mobile_video_close(self.native.as_ptr()) };
        }
    }
}
unsafe extern "C" {
    fn weld_probe_video_open() -> *mut c_void;
    fn weld_mobile_video_close(video: *mut c_void);
    fn weld_probe_video_texture(width: i32, height: i32) -> u32;
    fn weld_mobile_video_draw(
        video: *mut c_void,
        buffer: *mut c_void,
        target: u32,
        width: i32,
        height: i32,
        crop: *const f32,
    ) -> i32;
}
