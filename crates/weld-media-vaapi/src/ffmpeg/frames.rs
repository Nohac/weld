//! FFmpeg-owned frame storage, CPU uploads and borrowed DRM mappings.
//!
//! Mapped AVFrames retain their source frame. Exported Weld descriptors duplicate
//! file descriptors; the caller keeps the mapping alive until GPU consumption ends.

use super::{BufferRef, check};
use crate::{VaapiDmabuf, VaapiDmabufObject, VaapiDmabufPlane};
use anyhow::{Context, Result, bail, ensure};
use ffmpeg_next::ffi;
use std::{
    ffi::c_void,
    mem,
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    ptr,
};

pub(super) struct Frame(*mut ffi::AVFrame);

impl Frame {
    /// Give FFmpeg ownership of packed pixels without another CPU copy.
    pub(super) fn packed_bgra(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self> {
        let row = usize::try_from(width)?
            .checked_mul(4)
            .context("BGRA row overflow")?;
        let length = row
            .checked_mul(usize::try_from(height)?)
            .context("BGRA size overflow")?;
        ensure!(
            width > 0 && height > 0 && pixels.len() == length,
            "invalid packed BGRA extent or length"
        );
        let width = i32::try_from(width)?;
        let height = i32::try_from(height)?;
        let stride = i32::try_from(row)?;
        let frame = Self::new()?;
        let mut owner = Box::new(pixels);
        let pixels = owner.as_mut_ptr();
        let owner = Box::into_raw(owner);
        // SAFETY: owner uniquely owns length initialized pixel bytes. FFmpeg retains
        // them read-only and calls free_packed_pixels once on the final unref.
        let buffer = unsafe {
            ffi::av_buffer_create(
                pixels,
                length,
                Some(free_packed_pixels),
                owner.cast(),
                ffi::AV_BUFFER_FLAG_READONLY,
            )
        };
        if buffer.is_null() {
            // SAFETY: av_buffer_create did not accept the allocation on failure.
            unsafe { drop(Box::from_raw(owner)) };
            bail!("could not retain packed BGRA pixels");
        }
        // SAFETY: frame is unique; buf[0] owns data[0] for every declared row.
        let data = unsafe { &mut *frame.0 };
        data.format = ffi::AVPixelFormat::AV_PIX_FMT_BGR0 as i32;
        data.width = width;
        data.height = height;
        data.linesize[0] = stride;
        data.data[0] = pixels;
        data.buf[0] = buffer;
        set_rgb_properties(data, 0);
        Ok(frame)
    }

    pub(super) fn upload(&self, frames: &BufferRef) -> Result<Self> {
        let frame = Self::new()?;
        // SAFETY: frames is an initialized hardware pool and frame is empty.
        check(
            unsafe { ffi::av_hwframe_get_buffer(frames.0, frame.0, 0) },
            "could not allocate upload frame",
        )?;
        // SAFETY: self owns CPU pixels matching the pool's format and dimensions.
        check(
            unsafe { ffi::av_hwframe_transfer_data(frame.0, self.0, 0) },
            "could not upload BGRA frame",
        )?;
        // SAFETY: both frames are live; copying properties retains referenced metadata.
        check(
            unsafe { ffi::av_frame_copy_props(frame.0, self.0) },
            "could not copy uploaded frame properties",
        )?;
        Ok(frame)
    }

    /// Map for reading without a CPU pixel transfer. The mapping owns a source reference.
    pub(super) fn map_drm(&self) -> Result<Self> {
        let frame = Self::new()?;
        // SAFETY: frame is uniquely owned and empty.
        unsafe { (*frame.0).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32 };
        // SAFETY: both frames are live. READ synchronizes VA-API work before export;
        // DIRECT rejects mappings requiring a copy. FFmpeg owns mapping cleanup.
        check(
            unsafe {
                ffi::av_hwframe_map(
                    frame.0,
                    self.0,
                    ffi::AV_HWFRAME_MAP_READ as i32 | ffi::AV_HWFRAME_MAP_DIRECT as i32,
                )
            },
            "could not map VA-API frame to DRM PRIME",
        )?;
        Ok(frame)
    }

    /// Attach the graph's DRM context without dropping the mapping's source lease.
    pub(super) fn set_drm_input(&mut self, frames: &BufferRef, pts: i64) -> Result<()> {
        let reference = frames.try_clone()?;
        // SAFETY: this AVFrame is uniquely owned. buf[0] retains the original mapping
        // and source frame independently of this public hardware-context reference.
        unsafe {
            let data = &mut *self.0;
            ffi::av_buffer_unref(&mut data.hw_frames_ctx);
            data.hw_frames_ctx = reference.into_raw();
            set_rgb_properties(data, pts);
        }
        Ok(())
    }

    /// Duplicate mapped descriptors for Weld's owned DMA-BUF contract.
    pub(super) fn dmabuf(&self) -> Result<VaapiDmabuf> {
        // SAFETY: self owns a live AVFrame for this whole conversion.
        let frame = unsafe { &*self.0 };
        ensure!(
            frame.format == ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32
                && !frame.data[0].is_null(),
            "FFmpeg frame is not a DRM mapping"
        );
        // SAFETY: data[0] is an AVDRMFrameDescriptor for a mapped DRM_PRIME frame.
        let descriptor = unsafe { &*frame.data[0].cast::<ffi::AVDRMFrameDescriptor>() };
        let (fourcc, planes) = drm_layout(descriptor)?;
        let count = usize::try_from(descriptor.nb_objects)?;
        ensure!((1..=4).contains(&count), "invalid DRM object count");
        let objects = descriptor.objects[..count]
            .iter()
            .map(|object| {
                ensure!(object.fd >= 0, "invalid mapped DMA-BUF descriptor");
                // SAFETY: this descriptor is owned by the live FFmpeg mapping. Only the
                // duplicate escapes; neither the original FD nor its ownership is taken.
                let borrowed = unsafe { BorrowedFd::borrow_raw(object.fd) };
                Ok(VaapiDmabufObject {
                    file_descriptor: borrowed.try_clone_to_owned()?,
                    size: u32::try_from(object.size)?,
                    modifier: object.format_modifier,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        VaapiDmabuf::try_new(
            u32::try_from(frame.width)?,
            u32::try_from(frame.height)?,
            fourcc,
            objects,
            planes,
        )
    }

    pub(super) fn new() -> Result<Self> {
        // SAFETY: frame allocation has no caller-side preconditions.
        let frame = unsafe { ffi::av_frame_alloc() };
        ensure!(!frame.is_null(), "could not allocate FFmpeg frame");
        Ok(Self(frame))
    }

    pub(super) fn from_dmabuf(input: VaapiDmabuf, frames: &BufferRef, pts: i64) -> Result<Self> {
        ensure!(
            input.objects.len() <= 4 && input.planes.len() <= 4,
            "DMA-BUF exceeds FFmpeg DRM descriptor limits"
        );
        let width = i32::try_from(input.width)?;
        let height = i32::try_from(input.height)?;
        let frame = Self::new()?;
        let owner = Box::new(DrmDescriptorOwner::new(
            input.objects,
            input.planes,
            input.fourcc,
        )?);
        let owner = Box::into_raw(owner);
        // SAFETY: owner is a unique Box allocation retained by the callback on success.
        let buffer = unsafe {
            ffi::av_buffer_create(
                owner.cast::<u8>(),
                mem::size_of::<DrmDescriptorOwner>(),
                Some(free_drm_descriptor),
                ptr::null_mut(),
                0,
            )
        };
        if buffer.is_null() {
            // SAFETY: FFmpeg did not accept owner, so this reconstructs its unique Box.
            unsafe { drop(Box::from_raw(owner)) };
            bail!("could not create FFmpeg-owned DRM descriptor");
        }

        // SAFETY: frame.0 is uniquely owned and initialized by av_frame_alloc.
        let frame_ref = unsafe { &mut *frame.0 };
        frame_ref.format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        frame_ref.width = width;
        frame_ref.height = height;
        set_rgb_properties(frame_ref, pts);
        frame_ref.buf[0] = buffer;
        frame_ref.data[0] = owner.cast::<u8>();
        frame_ref.hw_frames_ctx = frames.try_clone()?.into_raw();
        Ok(frame)
    }

    pub(super) fn as_ptr(&self) -> *const ffi::AVFrame {
        self.0
    }

    pub(super) fn as_mut_ptr(&mut self) -> *mut ffi::AVFrame {
        self.0
    }

    pub(super) fn timestamp(&self) -> Result<u64> {
        // SAFETY: self.0 is a live AVFrame.
        let frame = unsafe { &*self.0 };
        let timestamp = if frame.pts != i64::MIN {
            frame.pts
        } else {
            frame.best_effort_timestamp
        };
        ensure!(timestamp != i64::MIN, "decoded frame has no timestamp");
        Ok(u64::try_from(timestamp)?)
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: self.0 is uniquely owned and may be nullified by FFmpeg.
        unsafe { ffi::av_frame_free(&mut self.0) };
    }
}

#[repr(C)]
struct DrmDescriptorOwner {
    descriptor: ffi::AVDRMFrameDescriptor,
    file_descriptors: Vec<OwnedFd>,
}

impl DrmDescriptorOwner {
    fn new(
        objects: Vec<VaapiDmabufObject>,
        planes: Vec<VaapiDmabufPlane>,
        fourcc: u32,
    ) -> Result<Self> {
        let mut descriptor = empty_drm_descriptor();
        descriptor.nb_objects = i32::try_from(objects.len())?;
        descriptor.nb_layers = 1;
        descriptor.layers[0].format = fourcc;
        descriptor.layers[0].nb_planes = i32::try_from(planes.len())?;
        let mut file_descriptors = Vec::with_capacity(objects.len());
        for (index, object) in objects.into_iter().enumerate() {
            descriptor.objects[index] = ffi::AVDRMObjectDescriptor {
                fd: object.file_descriptor.as_raw_fd(),
                size: usize::try_from(object.size)?,
                format_modifier: object.modifier,
            };
            file_descriptors.push(object.file_descriptor);
        }
        for (index, plane) in planes.into_iter().enumerate() {
            descriptor.layers[0].planes[index] = ffi::AVDRMPlaneDescriptor {
                object_index: i32::from(plane.object_index),
                offset: isize::try_from(plane.offset)?,
                pitch: isize::try_from(plane.stride)?,
            };
        }
        Ok(Self {
            descriptor,
            file_descriptors,
        })
    }
}

unsafe extern "C" fn free_drm_descriptor(_opaque: *mut c_void, data: *mut u8) {
    if !data.is_null() {
        // SAFETY: data was created by Box::into_raw in Frame::from_dmabuf and FFmpeg calls this once.
        unsafe { drop(Box::from_raw(data.cast::<DrmDescriptorOwner>())) };
    }
}

fn empty_drm_descriptor() -> ffi::AVDRMFrameDescriptor {
    let object = ffi::AVDRMObjectDescriptor {
        fd: -1,
        size: 0,
        format_modifier: 0,
    };
    let plane = ffi::AVDRMPlaneDescriptor {
        object_index: 0,
        offset: 0,
        pitch: 0,
    };
    let layer = ffi::AVDRMLayerDescriptor {
        format: 0,
        nb_planes: 0,
        planes: [plane; 4],
    };
    ffi::AVDRMFrameDescriptor {
        nb_objects: 0,
        objects: [object; 4],
        nb_layers: 0,
        layers: [layer; 4],
    }
}

unsafe extern "C" fn free_packed_pixels(opaque: *mut c_void, _data: *mut u8) {
    if !opaque.is_null() {
        // SAFETY: packed_bgra transfers this exact Box<Vec<u8>> to the AVBuffer.
        // FFmpeg calls this callback once after all references have been released.
        unsafe { drop(Box::from_raw(opaque.cast::<Vec<u8>>())) };
    }
}

fn set_rgb_properties(frame: &mut ffi::AVFrame, pts: i64) {
    frame.pts = pts;
    frame.time_base = ffi::AVRational {
        num: 1,
        den: 1_000_000,
    };
    frame.sample_aspect_ratio = ffi::AVRational { num: 1, den: 1 };
    frame.color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
    frame.colorspace = ffi::AVColorSpace::AVCOL_SPC_RGB;
    frame.color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
    frame.color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1;
}

/// FFmpeg exports separate R8/GR88 layers for NV12, while Weld/libva import
/// uses one NV12 layer with two planes. Preserve object indices and offsets.
fn drm_layout(descriptor: &ffi::AVDRMFrameDescriptor) -> Result<(u32, Vec<VaapiDmabufPlane>)> {
    const XRGB: u32 = u32::from_le_bytes(*b"XR24");
    const ARGB: u32 = u32::from_le_bytes(*b"AR24");
    const NV12: u32 = u32::from_le_bytes(*b"NV12");
    const R8: u32 = u32::from_le_bytes(*b"R8  ");
    const GR88: u32 = u32::from_le_bytes(*b"GR88");
    let count = usize::try_from(descriptor.nb_layers)?;
    ensure!((1..=4).contains(&count), "invalid DRM layer count");
    let layers = &descriptor.layers[..count];
    let fourcc = match layers {
        [layer] if matches!(layer.format, XRGB | ARGB) && layer.nb_planes == 1 => layer.format,
        [layer] if layer.format == NV12 && layer.nb_planes == 2 => NV12,
        [luma, chroma]
            if luma.format == R8
                && chroma.format == GR88
                && luma.nb_planes == 1
                && chroma.nb_planes == 1 =>
        {
            NV12
        }
        _ => bail!("unsupported DRM mapping layout"),
    };
    let planes = layers
        .iter()
        .flat_map(|layer| layer.planes.iter().take(layer.nb_planes as usize))
        .map(|plane| {
            ensure!(
                plane.object_index >= 0
                    && plane.object_index < descriptor.nb_objects
                    && plane.pitch > 0,
                "invalid mapped DRM plane"
            );
            Ok(VaapiDmabufPlane {
                object_index: u8::try_from(plane.object_index)?,
                offset: u32::try_from(plane.offset)?,
                stride: u32::try_from(plane.pitch)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((fourcc, planes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_pixels_transfer_ownership_without_copying() {
        let pixels: Vec<u8> = (0..24).collect();
        let original_pointer = pixels.as_ptr();
        let expected = pixels.clone();
        let frame = Frame::packed_bgra(3, 2, pixels).expect("CPU frame");
        // SAFETY: this frame owns the allocation created by packed_bgra.
        let data = unsafe { &*frame.as_ptr() };
        assert_eq!(data.data[0].cast_const(), original_pointer);
        let stride = usize::try_from(data.linesize[0]).expect("stride");
        for y in 0..2 {
            // SAFETY: each allocated row has at least width * 4 initialized bytes.
            let row = unsafe { std::slice::from_raw_parts(data.data[0].add(y * stride), 12) };
            assert_eq!(row, &expected[y * 12..(y + 1) * 12]);
        }
        assert!(Frame::packed_bgra(0, 2, vec![]).is_err());
        assert!(Frame::packed_bgra(3, 2, expected[..23].to_vec()).is_err());
        assert!(Frame::packed_bgra(u32::MAX, u32::MAX, vec![]).is_err());
    }

    #[test]
    fn separate_nv12_layers_keep_offsets_strides_and_object_indices() {
        let mut descriptor = empty_drm_descriptor();
        descriptor.nb_objects = 2;
        descriptor.nb_layers = 2;
        for (index, (format, offset, stride)) in [
            (u32::from_le_bytes(*b"R8  "), 64, 256),
            (u32::from_le_bytes(*b"GR88"), 128, 512),
        ]
        .into_iter()
        .enumerate()
        {
            descriptor.layers[index].format = format;
            descriptor.layers[index].nb_planes = 1;
            descriptor.layers[index].planes[0] = ffi::AVDRMPlaneDescriptor {
                object_index: index as i32,
                offset,
                pitch: stride,
            };
        }
        let (fourcc, planes) = drm_layout(&descriptor).expect("NV12");
        assert_eq!(fourcc, u32::from_le_bytes(*b"NV12"));
        assert_eq!(
            (planes[0].object_index, planes[0].offset, planes[0].stride),
            (0, 64, 256)
        );
        assert_eq!(
            (planes[1].object_index, planes[1].offset, planes[1].stride),
            (1, 128, 512)
        );
        descriptor.layers[1].planes[0].object_index = 2;
        assert!(drm_layout(&descriptor).is_err());
        descriptor.layers[1].planes[0].object_index = 1;
        descriptor.layers[1].planes[0].pitch = -1;
        assert!(drm_layout(&descriptor).is_err());
        descriptor.layers[1].planes[0].pitch = 512;
        descriptor.layers[1].format = u32::from_le_bytes(*b"RG88");
        assert!(drm_layout(&descriptor).is_err());
        descriptor.nb_layers = 5;
        assert!(drm_layout(&descriptor).is_err());
    }

    #[test]
    fn composed_layouts_validate_plane_count_and_offsets() {
        let mut descriptor = empty_drm_descriptor();
        descriptor.nb_objects = 1;
        descriptor.nb_layers = 1;
        descriptor.layers[0].format = u32::from_le_bytes(*b"NV12");
        descriptor.layers[0].nb_planes = 2;
        for plane in &mut descriptor.layers[0].planes[..2] {
            plane.pitch = 128;
        }
        assert_eq!(drm_layout(&descriptor).expect("composed NV12").1.len(), 2);
        descriptor.layers[0].format = u32::from_le_bytes(*b"XR24");
        assert!(drm_layout(&descriptor).is_err());
        descriptor.layers[0].nb_planes = 1;
        assert_eq!(drm_layout(&descriptor).expect("XRGB").1.len(), 1);
        descriptor.layers[0].planes[0].offset = -1;
        assert!(drm_layout(&descriptor).is_err());
    }
}
