//! GLES conversion shares Bevy's device/context and submission ordering.
use anyhow::{Context, Result, ensure};
use bevy::{
    prelude::*,
    render::{render_asset::RenderAssets, renderer::RenderDevice, texture::GpuImage},
    ui_render::ImageNodeBindGroups,
};
use std::{
    ffi::c_void,
    num::NonZeroU32,
    os::fd::{FromRawFd, OwnedFd},
    ptr::NonNull,
};
use wgpu::{Extent3d, TextureDimension, TextureFormat, TextureUsages, TextureUses};

use super::{Stream, receiver::Frame};

struct Converter {
    native: NonNull<c_void>,
    device: wgpu::Device,
    output: Option<(NonZeroU32, GpuImage)>,
    reported_synchronous_release: bool,
}
// SAFETY: native handles have no thread affinity; every use and destruction
// holds this retained device's EGL context lock, which makes it current.
unsafe impl Send for Converter {}
// SAFETY: shared references only retain handles. Mutation requires &mut self;
// GL access is additionally serialized by the device's context lock.
unsafe impl Sync for Converter {}

#[derive(Resource, Default)]
pub(super) struct VideoRenderer {
    converter: Option<Converter>,
    failed: bool,
    frames: u64,
}
impl Converter {
    fn new(device: &RenderDevice) -> Result<Self> {
        // SAFETY: guard only exposes this live device, without HAL ownership transfer.
        let hal = unsafe { device.wgpu_device().as_hal::<wgpu::hal::api::Gles>() }
            .context("phone native video requires GLES")?;
        let _context = hal.context().lock();
        // SAFETY: wgpu's EGL context is current and locked for this entire call.
        let native = NonNull::new(unsafe { weld_mobile_video_open() })
            .context("Android EGL video conversion unavailable")?;
        Ok(Self {
            native,
            device: device.wgpu_device().clone(),
            output: None,
            reported_synchronous_release: false,
        })
    }
    fn convert(&mut self, frame: Frame) -> Result<(GpuImage, weld_client::SurfaceInputGeometry)> {
        let info = frame.image.info();
        let view = frame.view;
        ensure!(
            view.source_x.is_finite()
                && view.source_y.is_finite()
                && view.source_width.is_finite()
                && view.source_height.is_finite()
                && view.source_x >= 0.0
                && view.source_y >= 0.0
                && view.source_width >= 1.0
                && view.source_height >= 1.0,
            "invalid video crop"
        );
        let left = info.crop[0] as f32 + view.source_x;
        let top = info.crop[1] as f32 + view.source_y;
        ensure!(
            left + view.source_width <= info.crop[2] as f32
                && top + view.source_height <= info.crop[3] as f32,
            "video crop exceeds native image"
        );
        let width = view.source_width.ceil() as u32;
        let height = view.source_height.ceil() as u32;
        let crop = [
            left / info.width as f32,
            top / info.height as f32,
            view.source_width / info.width as f32,
            view.source_height / info.height as f32,
        ];
        let descriptor = wgpu::TextureDescriptor {
            label: Some("mobile decoded video"),
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8UnormSrgb,
            usage: TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        if self
            .output
            .as_ref()
            .is_none_or(|(_, image)| image.texture_descriptor.size != descriptor.size)
        {
            let raw = {
                // SAFETY: retained device is the one which owns the converter.
                let hal = unsafe { self.device.as_hal::<wgpu::hal::api::Gles>() }
                    .context("GLES device missing")?;
                let _context = hal.context().lock();
                // SAFETY: positive dimensions checked against image bounds; current locked context.
                let name = NonZeroU32::new(unsafe {
                    weld_mobile_video_texture(i32::try_from(width)?, i32::try_from(height)?)
                })
                .context("video texture allocation failed")?;
                let desc = wgpu::hal::TextureDescriptor {
                    label: descriptor.label,
                    size: descriptor.size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: TextureDimension::D2,
                    format: descriptor.format,
                    usage: TextureUses::RESOURCE,
                    memory_flags: wgpu::hal::MemoryFlags::empty(),
                    view_formats: vec![],
                };
                // SAFETY: C allocated exactly this sRGB 2D texture on the same
                // context. HAL takes deletion ownership; C keeps only a borrowed name.
                (name, unsafe { hal.texture_from_raw(name, &desc, None) })
            };
            // SAFETY: same device/descriptor; first full conversion initializes
            // the texture before it is published for any Bevy sampling.
            let texture = unsafe {
                self.device.create_texture_from_hal::<wgpu::hal::api::Gles>(
                    raw.1,
                    &descriptor,
                    TextureUses::RESOURCE,
                )
            };
            let texture_view = texture.create_view(&Default::default());
            let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            self.output = Some((
                raw.0,
                GpuImage {
                    texture: texture.into(),
                    texture_view: texture_view.into(),
                    sampler: sampler.into(),
                    texture_descriptor: descriptor,
                    texture_view_descriptor: None,
                    had_data: false,
                },
            ));
        }
        let (target, image) = self.output.as_ref().context("video target missing")?;
        let fence = {
            // SAFETY: guards scope all native GL access; no wgpu calls while locked.
            let hal = unsafe { self.device.as_hal::<wgpu::hal::api::Gles>() }
                .context("GLES device missing")?;
            let _context = hal.context().lock();
            // SAFETY: frame retains the acquired image until the exported fence
            // is transferred to Android below. C synchronizes reads on failure.
            unsafe {
                weld_mobile_video_draw(
                    self.native.as_ptr(),
                    frame.image.hardware_buffer_ptr()?,
                    target.get(),
                    i32::try_from(width)?,
                    i32::try_from(height)?,
                    crop.as_ptr(),
                )
            }
        };
        if fence >= 0 {
            // SAFETY: successful draw transfers one newly owned native fence FD.
            frame
                .image
                .release_after(unsafe { OwnedFd::from_raw_fd(fence) });
        } else {
            ensure!(fence == -2, "video conversion or release fence failed");
            if !self.reported_synchronous_release {
                warn!(
                    "native video fence export failed; synchronized GPU completion before releasing image"
                );
                self.reported_synchronous_release = true;
            }
            drop(frame.image);
        }
        Ok((image.clone(), frame.input))
    }
}
impl Drop for Converter {
    fn drop(&mut self) {
        // SAFETY: device retained through native cleanup; objects were created
        // on precisely this context. Output textures retain their own HAL owner.
        if let Some(hal) = unsafe { self.device.as_hal::<wgpu::hal::api::Gles>() } {
            let _context = hal.context().lock();
            // SAFETY: unique live converter and its current locked context.
            unsafe { weld_mobile_video_close(self.native.as_ptr()) };
        }
    }
}

pub(super) fn present(
    stream: Option<Res<Stream>>,
    device: Res<RenderDevice>,
    mut state: ResMut<VideoRenderer>,
    mut images: ResMut<RenderAssets<GpuImage>>,
    mut bindings: ResMut<ImageNodeBindGroups>,
) {
    let Some(stream) = stream else {
        return;
    };
    if state.failed {
        return;
    }
    let frame = stream
        .shared
        .latest
        .lock()
        .ok()
        .and_then(|mut latest| latest.take());
    let Some(frame) = frame else {
        return;
    };
    let epoch = frame.epoch;
    let result = (|| {
        if state.converter.is_none() {
            state.converter = Some(Converter::new(&device)?);
        }
        state
            .converter
            .as_mut()
            .context("converter missing")?
            .convert(frame)
    })();
    match result {
        Ok((image, input)) => {
            if let Ok(mut displayed) = stream.shared.displayed.lock() {
                if epoch
                    != stream
                        .shared
                        .epoch
                        .load(std::sync::atomic::Ordering::Acquire)
                {
                    return;
                }
                if images
                    .get(stream.image.id())
                    .is_none_or(|current| current.texture.id() != image.texture.id())
                {
                    bindings.values.remove(&stream.image.id());
                }
                images.insert(stream.image.id(), image);
                *displayed = Some((epoch, input));
            }
            state.frames += 1;
            if state.frames == 1 || state.frames.is_multiple_of(300) {
                info!(
                    frames = state.frames,
                    "Android video presented through Bevy (GPU-only conversion)"
                );
            }
        }
        Err(error) => {
            error!("native presentation failed: {error:#}");
            if let Ok(mut status) = stream.shared.status.lock() {
                *status = format!("Video failed: {error:#}");
            }
            stream
                .shared
                .stopped
                .store(true, std::sync::atomic::Ordering::Release);
            state.failed = true;
        }
    }
}

unsafe extern "C" {
    fn weld_mobile_video_open() -> *mut c_void;
    fn weld_mobile_video_close(video: *mut c_void);
    fn weld_mobile_video_texture(width: i32, height: i32) -> u32;
    fn weld_mobile_video_draw(
        video: *mut c_void,
        buffer: *mut c_void,
        target: u32,
        width: i32,
        height: i32,
        crop: *const f32,
    ) -> i32;
}
