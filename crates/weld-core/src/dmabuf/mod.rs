//! Linux DMA-BUF capability discovery and GPU import.

mod device;
mod manager;
mod source;

pub use device::{DmabufCapabilities, request_weld_device};
pub use manager::{
    DmabufContext, DmabufManager, ImportedImageRegistry, PromotionImage, StagedImport,
};
pub(crate) use source::{DmabufSourceCache, ImportedDmabufSource};

use smithay::backend::allocator::dmabuf::Dmabuf;
use std::os::fd::OwnedFd;
use weld_client::{ClientBufferLease, ClientBufferUseId, Extent};

/// One owned DMA-BUF plane crossing an adapter or process boundary.
#[derive(Debug)]
pub struct ExternalDmabufPlane {
    pub file_descriptor: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// Native DMA-BUF description independent from Smithay protocol ownership.
#[derive(Debug)]
pub struct ExternalDmabuf {
    pub extent: Extent,
    pub format: u32,
    pub modifier: u64,
    pub flags: u32,
    pub planes: Vec<ExternalDmabufPlane>,
}

impl ExternalDmabuf {
    fn into_smithay(self) -> anyhow::Result<Dmabuf> {
        use anyhow::{Context, bail};
        use smithay::backend::allocator::{Fourcc, Modifier, dmabuf::DmabufFlags};

        if self.extent.width == 0 || self.extent.height == 0 {
            bail!("zero-sized external DMA-BUF");
        }
        let width = i32::try_from(self.extent.width).context("external DMA-BUF width overflow")?;
        let height =
            i32::try_from(self.extent.height).context("external DMA-BUF height overflow")?;
        let format = Fourcc::try_from(self.format).context("unrecognized DMA-BUF format")?;
        let mut builder = Dmabuf::builder(
            (width, height),
            format,
            Modifier::from(self.modifier),
            DmabufFlags::from_bits_retain(self.flags),
        );
        for plane in self.planes {
            if !builder.add_plane(plane.file_descriptor, plane.offset, plane.stride) {
                bail!("external DMA-BUF has too many planes");
            }
        }
        builder.build().context("external DMA-BUF has no planes")
    }
}

/// Duplicates the native descriptors carried by one direct client-buffer lease.
pub fn export_client_dmabuf(lease: &ClientBufferLease) -> anyhow::Result<ExternalDmabuf> {
    use anyhow::Context;
    use smithay::backend::allocator::Buffer;

    let access = lease
        .access::<DirectClientBufferAccess>()
        .context("client-buffer lease does not contain direct native access")?;
    let DirectClientBufferAccess::Dmabuf(access) = access else {
        anyhow::bail!("client-buffer lease contains copied SHM pixels, not a DMA-BUF");
    };
    let size = access.dmabuf.size();
    let extent = Extent::new(
        u32::try_from(size.w).context("negative DMA-BUF width")?,
        u32::try_from(size.h).context("negative DMA-BUF height")?,
    );
    let planes = access
        .dmabuf
        .handles()
        .zip(access.dmabuf.offsets())
        .zip(access.dmabuf.strides())
        .map(|((file_descriptor, offset), stride)| {
            Ok(ExternalDmabufPlane {
                file_descriptor: file_descriptor
                    .try_clone_to_owned()
                    .context("failed to duplicate DMA-BUF plane")?,
                offset,
                stride,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let format = access.dmabuf.format();
    Ok(ExternalDmabuf {
        extent,
        format: format.code as u32,
        modifier: format.modifier.into(),
        flags: access.dmabuf.flags().bits(),
        planes,
    })
}

/// Native access payload resolved by Weld's built-in application importer.
#[derive(Debug)]
pub enum DirectClientBufferAccess {
    Shm(WaylandShmBuffer),
    Dmabuf(DmabufAccess),
}

/// One already-copied SHM buffer retained until application import.
#[derive(Debug)]
pub struct WaylandShmBuffer {
    pub bgra_pixels: Vec<u8>,
}

/// Stable identity of one imported live Wayland buffer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ImportId(u64);

impl ImportId {
    pub(crate) const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) const fn next(self) -> Option<u64> {
        self.0.checked_add(1)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub const fn for_test(raw: u64) -> Self {
        Self(raw)
    }
}

/// Server-owned identity returned only after GPU consumption has completed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DmabufReleaseId(u64);

impl DmabufReleaseId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum DmabufEvent {
    GpuUseCompleted(ClientBufferUseId),
    LeaseCompleted(DmabufReleaseId),
}

/// A validated DMA-BUF crossing into the shell renderer.
#[derive(Clone, Debug)]
pub struct DmabufAccess {
    dmabuf: Dmabuf,
}

impl DmabufAccess {
    pub(crate) fn new(dmabuf: Dmabuf) -> Self {
        Self { dmabuf }
    }
}

/// One Wayland DMA-BUF use awaiting neutral lease construction.
#[derive(Debug)]
pub struct PendingWaylandDmabufUse {
    access: DmabufAccess,
    release: DmabufReleaseId,
}

impl PendingWaylandDmabufUse {
    pub(crate) fn new(dmabuf: Dmabuf, release: DmabufReleaseId) -> Self {
        Self {
            access: DmabufAccess::new(dmabuf),
            release,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;
    use weld_client::{ClientBufferId, ClientBufferMetadata, ClientSourceId};

    #[test]
    fn copied_shm_cannot_enter_the_native_export_path() {
        let source = ClientSourceId::new(1);
        let lease = ClientBufferLease::new(
            ClientBufferId::new(source, 2),
            ClientBufferUseId::new(source, 3),
            ClientBufferMetadata::new(Extent::new(1, 1), false),
            Rc::new(DirectClientBufferAccess::Shm(WaylandShmBuffer {
                bgra_pixels: vec![0; 4],
            })),
            |_| {},
        )
        .expect("matching source");

        assert!(export_client_dmabuf(&lease).is_err());
    }
}
