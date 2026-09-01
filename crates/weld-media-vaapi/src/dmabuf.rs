use std::os::fd::{AsRawFd, OwnedFd};

use anyhow::{Context, Result, ensure};
use cros_libva::{
    DrmPrimeSurfaceDescriptor, SurfaceMemoryDescriptor, VA_FOURCC_BGRX, VA_FOURCC_NV12,
    VADRMPRIMESurfaceDescriptor, VADRMPRIMESurfaceDescriptorLayer,
    VADRMPRIMESurfaceDescriptorObject, VASurfaceAttrib,
};

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
    pub va_fourcc: u32,
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
        let va_fourcc = match fourcc {
            DRM_FORMAT_ARGB8888 | DRM_FORMAT_XRGB8888 => VA_FOURCC_BGRX,
            DRM_FORMAT_NV12 => VA_FOURCC_NV12,
            _ => anyhow::bail!("DRM format is not supported by the VA-API tracer"),
        };
        let frame = Self {
            width,
            height,
            va_fourcc,
            fourcc,
            objects,
            planes,
        };
        frame.validate()?;
        Ok(frame)
    }

    pub(crate) fn from_prime(descriptor: DrmPrimeSurfaceDescriptor) -> Result<Self> {
        ensure!(
            descriptor.layers.len() == 1,
            "VA-API export did not produce one composed layer"
        );
        let layer = descriptor
            .layers
            .first()
            .context("VA-API export has no layer")?;
        let plane_count = usize::try_from(layer.num_planes)
            .context("VA-API plane count exceeds address space")?;
        ensure!(plane_count <= 4, "VA-API export has too many planes");
        let planes = (0..plane_count)
            .map(|index| VaapiDmabufPlane {
                object_index: layer.object_index[index],
                offset: layer.offset[index],
                stride: layer.pitch[index],
            })
            .collect();
        let objects = descriptor
            .objects
            .into_iter()
            .map(|object| VaapiDmabufObject {
                file_descriptor: object.fd,
                size: object.size,
                modifier: object.drm_format_modifier,
            })
            .collect();
        let frame = Self {
            width: descriptor.width,
            height: descriptor.height,
            va_fourcc: descriptor.fourcc,
            fourcc: layer.drm_format,
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
            va_fourcc: self.va_fourcc,
            fourcc: self.fourcc,
            objects,
            planes: self.planes.clone(),
        })
    }

    fn validate(&self) -> Result<()> {
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
        Ok(())
    }

    pub(crate) fn import_descriptor(&self) -> Result<PrimeImportDescriptor> {
        self.validate()?;
        Ok(PrimeImportDescriptor {
            frame: self.try_clone()?,
        })
    }
}

pub(crate) struct PrimeImportDescriptor {
    frame: VaapiDmabuf,
}

impl SurfaceMemoryDescriptor for PrimeImportDescriptor {
    fn add_attrs(
        &mut self,
        attributes: &mut Vec<VASurfaceAttrib>,
    ) -> Option<Box<dyn std::any::Any>> {
        let num_objects = u32::try_from(self.frame.objects.len()).ok()?;
        let num_planes = u32::try_from(self.frame.planes.len()).ok()?;
        let mut descriptor = Box::new(VADRMPRIMESurfaceDescriptor {
            fourcc: self.frame.va_fourcc,
            width: self.frame.width,
            height: self.frame.height,
            num_objects,
            num_layers: 1,
            ..Default::default()
        });
        for (index, object) in self.frame.objects.iter().enumerate() {
            descriptor.objects[index] = VADRMPRIMESurfaceDescriptorObject {
                fd: object.file_descriptor.as_raw_fd(),
                size: object.size,
                drm_format_modifier: object.modifier,
            };
        }
        let mut layer = VADRMPRIMESurfaceDescriptorLayer {
            drm_format: self.frame.fourcc,
            num_planes,
            ..Default::default()
        };
        for (index, plane) in self.frame.planes.iter().enumerate() {
            layer.object_index[index] = u32::from(plane.object_index);
            layer.offset[index] = plane.offset;
            layer.pitch[index] = plane.stride;
        }
        descriptor.layers[0] = layer;
        attributes.push(VASurfaceAttrib::new_memory_type(
            cros_libva::MemoryType::DrmPrime2,
        ));
        attributes.push(VASurfaceAttrib::new_buffer_descriptor(descriptor.as_mut()));
        Some(descriptor)
    }
}
