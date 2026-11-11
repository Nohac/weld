// Copyright 2022 The ChromiumOS Authors
// Adapted for Weld. See LICENSE and UPSTREAM.md.
use anyhow::{Context, Result, ensure};
use std::os::fd::OwnedFd;

const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");

#[derive(Debug)]
pub struct VaapiDmabufObject {
    pub file_descriptor: OwnedFd,
    pub size: u32,
    pub modifier: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VaapiDmabufPlane {
    pub object_index: u8,
    pub offset: u32,
    pub stride: u32,
}

/// Owned DMA-BUF exported from or importable by a VA-API media stage.
#[derive(Debug)]
pub struct VaapiDmabuf {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub objects: Vec<VaapiDmabufObject>,
    pub planes: Vec<VaapiDmabufPlane>,
}

impl VaapiDmabuf {
    /// Constructs a checked VA-API import from owned DRM DMA-BUF objects.
    pub fn try_new(
        width: u32,
        height: u32,
        fourcc: u32,
        objects: Vec<VaapiDmabufObject>,
        planes: Vec<VaapiDmabufPlane>,
    ) -> Result<Self> {
        let frame = Self {
            width,
            height,
            fourcc,
            objects,
            planes,
        };
        frame.validate()?;
        Ok(frame)
    }

    pub fn primary_modifier(&self) -> Result<u64> {
        self.objects
            .first()
            .map(|object| object.modifier)
            .context("DMA-BUF has no object")
    }

    pub fn try_clone(&self) -> Result<Self> {
        let objects = self
            .objects
            .iter()
            .map(|object| {
                Ok(VaapiDmabufObject {
                    file_descriptor: object
                        .file_descriptor
                        .try_clone()
                        .context("could not duplicate DMA-BUF object")?,
                    size: object.size,
                    modifier: object.modifier,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            width: self.width,
            height: self.height,
            fourcc: self.fourcc,
            objects,
            planes: self.planes.clone(),
        })
    }

    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            matches!(
                self.fourcc,
                DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 | DRM_FORMAT_NV12
            ),
            "unsupported DMA-BUF format"
        );
        let expected_planes = if self.fourcc == DRM_FORMAT_NV12 { 2 } else { 1 };
        ensure!(
            self.planes.len() == expected_planes,
            "DMA-BUF format has the wrong plane count"
        );
        ensure!(
            self.objects.iter().all(|object| object.size > 0),
            "DMA-BUF has an empty object"
        );
        ensure!(self.width > 0 && self.height > 0, "DMA-BUF has zero extent");
        ensure!(!self.objects.is_empty(), "DMA-BUF has no object");
        ensure!(!self.planes.is_empty(), "DMA-BUF has no plane");
        ensure!(self.objects.len() <= 4, "DMA-BUF has too many objects");
        ensure!(self.planes.len() <= 4, "DMA-BUF has too many planes");
        ensure!(
            self.planes
                .iter()
                .all(|plane| usize::from(plane.object_index) < self.objects.len()),
            "DMA-BUF plane references an absent object"
        );
        ensure!(
            self.planes.iter().all(|plane| plane.stride > 0
                && plane.offset < self.objects[usize::from(plane.object_index)].size),
            "DMA-BUF plane exceeds its object"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::File, os::fd::AsRawFd};
    fn object() -> VaapiDmabufObject {
        VaapiDmabufObject {
            file_descriptor: File::open("/dev/null").expect("file").into(),
            size: 4096,
            modifier: 0,
        }
    }
    #[test]
    fn dmabuf_clones_descriptors_and_rejects_invalid_layouts() {
        let plane = VaapiDmabufPlane {
            object_index: 0,
            offset: 0,
            stride: 64,
        };
        let frame = VaapiDmabuf::try_new(16, 16, DRM_FORMAT_XRGB8888, vec![object()], vec![plane])
            .expect("frame");
        let clone = frame.try_clone().expect("clone");
        assert_ne!(
            frame.objects[0].file_descriptor.as_raw_fd(),
            clone.objects[0].file_descriptor.as_raw_fd()
        );
        for (format, width, planes) in [
            (DRM_FORMAT_NV12, 16, vec![plane]),
            (DRM_FORMAT_XRGB8888, 0, vec![plane]),
            (
                DRM_FORMAT_XRGB8888,
                16,
                vec![VaapiDmabufPlane {
                    object_index: 1,
                    ..plane
                }],
            ),
            (
                DRM_FORMAT_XRGB8888,
                16,
                vec![VaapiDmabufPlane {
                    offset: 4096,
                    ..plane
                }],
            ),
            (
                DRM_FORMAT_XRGB8888,
                16,
                vec![VaapiDmabufPlane { stride: 0, ..plane }],
            ),
        ] {
            assert!(VaapiDmabuf::try_new(width, 16, format, vec![object()], planes).is_err());
        }
    }
}
