// Copyright 2022 The ChromiumOS Authors
// Adapted for Weld. See LICENSE and UPSTREAM.md.
use crate::{
    VaapiDmabuf, VaapiDmabufObject, VaapiDmabufPlane,
    device::{Display, check, cleanup, initialized},
    ffi,
};
use anyhow::{Context as _, Result, ensure};
use std::{
    ffi::c_void,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    ptr,
    rc::Rc,
};

pub(crate) enum SurfaceRole {
    Input,
    Output,
}

pub(crate) struct Surface {
    pub(crate) display: Rc<Display>,
    pub(crate) id: ffi::VASurfaceID,
    pub(crate) width: u32,
    pub(crate) height: u32,
    _import: Option<VaapiDmabuf>,
}
impl Surface {
    pub(crate) fn import(display: Rc<Display>, input: &VaapiDmabuf) -> Result<Self> {
        input.validate()?;
        let owned = input.try_clone()?;
        let va_fourcc = if owned.fourcc == u32::from_le_bytes(*b"NV12") {
            ffi::VA_FOURCC_NV12
        } else {
            ffi::VA_FOURCC_BGRX
        };
        let mut descriptor = ffi::VADRMPRIMESurfaceDescriptor {
            fourcc: va_fourcc,
            width: owned.width,
            height: owned.height,
            num_objects: u32::try_from(owned.objects.len())?,
            num_layers: 1,
            ..Default::default()
        };
        for (destination, source) in descriptor.objects.iter_mut().zip(&owned.objects) {
            destination.fd = source.file_descriptor.as_raw_fd();
            destination.size = source.size;
            destination.drm_format_modifier = source.modifier;
        }
        let layer = &mut descriptor.layers[0];
        layer.drm_format = owned.fourcc;
        layer.num_planes = u32::try_from(owned.planes.len())?;
        for (index, plane) in owned.planes.iter().enumerate() {
            layer.object_index[index] = u32::from(plane.object_index);
            layer.offset[index] = plane.offset;
            layer.pitch[index] = plane.stride;
        }
        let mut attrs = [
            integer(
                ffi::VASurfaceAttribType::VASurfaceAttribPixelFormat,
                va_fourcc,
            ),
            integer(
                ffi::VASurfaceAttribType::VASurfaceAttribMemoryType,
                ffi::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
            ),
            pointer(
                ffi::VASurfaceAttribType::VASurfaceAttribExternalBufferDescriptor,
                (&mut descriptor as *mut ffi::VADRMPRIMESurfaceDescriptor).cast(),
            ),
            integer(
                ffi::VASurfaceAttribType::VASurfaceAttribUsageHint,
                ffi::VA_SURFACE_ATTRIB_USAGE_HINT_VPP_READ,
            ),
        ];
        let format = if owned.fourcc == u32::from_le_bytes(*b"NV12") {
            ffi::VA_RT_FORMAT_YUV420
        } else {
            ffi::VA_RT_FORMAT_RGB32
        };
        let mut surface = Self::create(display, owned.width, owned.height, format, &mut attrs)?;
        surface._import = Some(owned);
        Ok(surface)
    }
    pub(crate) fn xrgb(
        display: Rc<Display>,
        width: u32,
        height: u32,
        modifiers: &[u64],
        role: SurfaceRole,
    ) -> Result<Self> {
        ensure!(
            !modifiers.is_empty(),
            "XRGB output requires modifier candidates"
        );
        let mut modifiers = modifiers.to_vec();
        let mut list = ffi::VADRMFormatModifierList {
            num_modifiers: u32::try_from(modifiers.len())?,
            modifiers: modifiers.as_mut_ptr(),
        };
        let mut attrs = [
            integer(
                ffi::VASurfaceAttribType::VASurfaceAttribPixelFormat,
                ffi::VA_FOURCC_BGRX,
            ),
            integer(
                ffi::VASurfaceAttribType::VASurfaceAttribUsageHint,
                (match role {
                    SurfaceRole::Input => ffi::VA_SURFACE_ATTRIB_USAGE_HINT_VPP_READ,
                    SurfaceRole::Output => ffi::VA_SURFACE_ATTRIB_USAGE_HINT_VPP_WRITE,
                }) | ffi::VA_SURFACE_ATTRIB_USAGE_HINT_EXPORT,
            ),
            pointer(
                ffi::VASurfaceAttribType::VASurfaceAttribDRMFormatModifiers,
                (&mut list as *mut ffi::VADRMFormatModifierList).cast(),
            ),
        ];
        Self::create(display, width, height, ffi::VA_RT_FORMAT_RGB32, &mut attrs)
    }
    fn create(
        display: Rc<Display>,
        width: u32,
        height: u32,
        format: u32,
        attributes: &mut [ffi::VASurfaceAttrib],
    ) -> Result<Self> {
        ensure!(width > 0 && height > 0, "VA surface has zero extent");
        let mut id = 0;
        // SAFETY: all attribute backing storage is alive for this synchronous call;
        // import() retains its duplicated DMA-BUF FDs for the surface lifetime.
        check(unsafe {
            ffi::vaCreateSurfaces(
                display.handle,
                format,
                width,
                height,
                &mut id,
                1,
                attributes.as_mut_ptr(),
                u32::try_from(attributes.len())?,
            )
        })?;
        Ok(Self {
            display,
            id,
            width,
            height,
            _import: None,
        })
    }
    pub(crate) fn sync(&self) -> Result<()> {
        // SAFETY: this guard owns the surface and its display.
        check(unsafe { ffi::vaSyncSurface(self.display.handle, self.id) })
    }
    pub(crate) fn export(&self) -> Result<VaapiDmabuf> {
        self.sync()?;
        let mut descriptor = ffi::VADRMPRIMESurfaceDescriptor::default();
        // SAFETY: the surface is complete, descriptor is writable, and successful
        // export transfers ownership of every returned FD to this call.
        check(unsafe {
            ffi::vaExportSurfaceHandle(
                self.display.handle,
                self.id,
                ffi::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                ffi::VA_EXPORT_SURFACE_READ_ONLY | ffi::VA_EXPORT_SURFACE_COMPOSED_LAYERS,
                (&mut descriptor as *mut ffi::VADRMPRIMESurfaceDescriptor).cast(),
            )
        })?;
        let objects = descriptor
            .objects
            .iter()
            .take(usize::try_from(descriptor.num_objects)?.min(4))
            .map(|object| {
                ensure!(object.fd >= 0, "VA-API exported an invalid FD");
                Ok(VaapiDmabufObject {
                    // SAFETY: a successful export creates a unique owned FD for each object.
                    file_descriptor: unsafe { OwnedFd::from_raw_fd(object.fd) },
                    size: object.size,
                    modifier: object.drm_format_modifier,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            (1..=4).contains(&descriptor.num_objects) && descriptor.num_layers == 1,
            "invalid VA-API export layout"
        );
        let layer = &descriptor.layers[0];
        ensure!(
            (1..=4).contains(&layer.num_planes),
            "invalid VA-API export plane count"
        );
        let planes = (0..usize::try_from(layer.num_planes)?)
            .map(|index| {
                Ok(VaapiDmabufPlane {
                    object_index: u8::try_from(layer.object_index[index])?,
                    offset: layer.offset[index],
                    stride: layer.pitch[index],
                })
            })
            .collect::<Result<_>>()?;
        VaapiDmabuf::try_new(
            descriptor.width,
            descriptor.height,
            layer.drm_format,
            objects,
            planes,
        )
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: unique surface ID; display and imported FDs remain alive through destruction.
        cleanup(
            unsafe { ffi::vaDestroySurfaces(self.display.handle, &mut self.id, 1) },
            "surface",
        );
    }
}
pub(crate) fn integer(kind: ffi::VASurfaceAttribType::Type, value: u32) -> ffi::VASurfaceAttrib {
    let mut attribute = ffi::VASurfaceAttrib {
        type_: kind,
        flags: ffi::VA_SURFACE_ATTRIB_SETTABLE,
        ..Default::default()
    };
    attribute.value.type_ = ffi::VAGenericValueType::VAGenericValueTypeInteger;
    attribute.value.value.i = value as i32;
    attribute
}
fn pointer(kind: ffi::VASurfaceAttribType::Type, value: *mut c_void) -> ffi::VASurfaceAttrib {
    let mut attribute = ffi::VASurfaceAttrib {
        type_: kind,
        flags: ffi::VA_SURFACE_ATTRIB_SETTABLE,
        ..Default::default()
    };
    attribute.value.type_ = ffi::VAGenericValueType::VAGenericValueTypePointer;
    attribute.value.value.p = value;
    attribute
}

/// The surface outlives the mapped image. Writeback is explicit and fallible.
pub(crate) struct Image<'a> {
    surface: &'a Surface,
    description: ffi::VAImage,
    mapped: *mut u8,
}
impl<'a> Image<'a> {
    pub(crate) fn new(surface: &'a Surface) -> Result<Self> {
        // SAFETY: the initialized display is retained by surface.
        let mut count = unsafe { ffi::vaMaxNumImageFormats(surface.display.handle) };
        ensure!(count > 0, "VA image-format capacity is invalid");
        let mut formats = initialized(usize::try_from(count)?)?;
        // SAFETY: formats has the required writable capacity; count is writable.
        check(unsafe {
            ffi::vaQueryImageFormats(surface.display.handle, formats.as_mut_ptr(), &mut count)
        })?;
        let count = usize::try_from(count)?;
        ensure!(count <= formats.len(), "too many VA image formats");
        let mut format = formats[..count]
            .iter()
            .find(|format| format.fourcc == ffi::VA_FOURCC_BGRX)
            .copied()
            .context("VA-API has no BGRX image format")?;
        let mut description = ffi::VAImage::default();
        // SAFETY: live display and initialized format, with a writable output record.
        check(unsafe {
            ffi::vaCreateImage(
                surface.display.handle,
                &mut format,
                i32::try_from(surface.width)?,
                i32::try_from(surface.height)?,
                &mut description,
            )
        })?;
        let mut image = Self {
            surface,
            description,
            mapped: ptr::null_mut(),
        };
        ensure!(
            u32::from(description.width) >= surface.width
                && u32::from(description.height) >= surface.height,
            "VA image is smaller than its surface"
        );
        // SAFETY: both objects are owned guards on this display, with matching extents.
        check(unsafe {
            ffi::vaGetImage(
                surface.display.handle,
                surface.id,
                0,
                0,
                surface.width,
                surface.height,
                description.image_id,
            )
        })?;
        let mut mapped = ptr::null_mut();
        // SAFETY: description.buf belongs to this live image; output pointer is writable.
        check(unsafe { ffi::vaMapBuffer(surface.display.handle, description.buf, &mut mapped) })?;
        image.mapped = mapped.cast();
        ensure!(!image.mapped.is_null(), "VA image mapping returned null");
        Ok(image)
    }
    pub(crate) fn pixels(&self) -> Result<Vec<u8>> {
        let mut pixels = Vec::new();
        let row = usize::try_from(self.surface.width)?
            .checked_mul(4)
            .context("image row overflow")?;
        pixels.try_reserve_exact(
            row.checked_mul(usize::try_from(self.surface.height)?)
                .context("image size overflow")?,
        )?;
        for y in 0..self.surface.height {
            let range = self.row(y, row)?;
            // SAFETY: vaGetImage initialized this mapping; row() checks its bounds.
            pixels.extend_from_slice(unsafe {
                std::slice::from_raw_parts(self.mapped.add(range.start), row)
            });
        }
        Ok(pixels)
    }
    pub(crate) fn write(&mut self, pixels: &[u8]) -> Result<()> {
        let row = usize::try_from(self.surface.width)?
            .checked_mul(4)
            .context("image row overflow")?;
        ensure!(
            pixels.len()
                == row
                    .checked_mul(usize::try_from(self.surface.height)?)
                    .context("image size overflow")?,
            "invalid BGRA byte length"
        );
        for (y, source) in pixels.chunks_exact(row).enumerate() {
            let range = self.row(u32::try_from(y)?, row)?;
            // SAFETY: row() checks writable mapping bounds, and source is a disjoint Rust allocation.
            unsafe { ptr::copy_nonoverlapping(source.as_ptr(), self.mapped.add(range.start), row) };
        }
        // SAFETY: the image and its written mapping remain live through submission,
        // matching the upstream mapped-image writeback lifetime.
        check(unsafe {
            ffi::vaPutImage(
                self.surface.display.handle,
                self.surface.id,
                self.description.image_id,
                0,
                0,
                self.surface.width,
                self.surface.height,
                0,
                0,
                self.surface.width,
                self.surface.height,
            )
        })?;
        self.unmap()
    }
    fn row(&self, y: u32, bytes: usize) -> Result<std::ops::Range<usize>> {
        ensure!(!self.mapped.is_null(), "VA image is not mapped");
        image_row(&self.description, self.surface.height, y, bytes)
    }
    fn unmap(&mut self) -> Result<()> {
        if self.mapped.is_null() {
            return Ok(());
        }
        // SAFETY: this guard owns the currently mapped image buffer.
        check(unsafe { ffi::vaUnmapBuffer(self.surface.display.handle, self.description.buf) })?;
        self.mapped = ptr::null_mut();
        Ok(())
    }
}
impl Drop for Image<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.unmap() {
            tracing::warn!(%error, "could not unmap VA image");
        }
        // SAFETY: unique image ID and its surface/display outlive this destructor.
        cleanup(
            unsafe { ffi::vaDestroyImage(self.surface.display.handle, self.description.image_id) },
            "image",
        );
    }
}

fn image_row(
    description: &ffi::VAImage,
    height: u32,
    y: u32,
    bytes: usize,
) -> Result<std::ops::Range<usize>> {
    ensure!(
        y < height && description.num_planes == 1,
        "invalid image row"
    );
    let pitch = usize::try_from(description.pitches[0])?;
    ensure!(pitch >= bytes, "VA image pitch is too small");
    let start = usize::try_from(description.offsets[0])?
        .checked_add(
            usize::try_from(y)?
                .checked_mul(pitch)
                .context("image offset overflow")?,
        )
        .context("image offset overflow")?;
    let end = start.checked_add(bytes).context("image row overflow")?;
    ensure!(
        end <= usize::try_from(description.data_size)?,
        "VA image row exceeds mapping"
    );
    Ok(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_rows_preserve_offset_and_pitch_and_reject_truncated_mappings() {
        let mut description = ffi::VAImage {
            num_planes: 1,
            data_size: 64,
            ..Default::default()
        };
        description.offsets[0] = 8;
        description.pitches[0] = 24;
        assert_eq!(
            image_row(&description, 2, 1, 12).expect("padded row"),
            32..44
        );
        assert!(image_row(&description, 2, 2, 12).is_err());
        assert!(image_row(&description, 2, 0, 25).is_err());
        description.data_size = 43;
        assert!(image_row(&description, 2, 1, 12).is_err());
        description.num_planes = 2;
        assert!(image_row(&description, 2, 0, 12).is_err());
    }
}
