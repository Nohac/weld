//! Linux DMA-BUF capability discovery and GPU import.

mod device;
mod manager;
mod source;

pub use device::{DmabufCapabilities, request_weld_device};
pub use manager::{
    DmabufContext, DmabufManager, ImportedImageRegistry, PromotionImage, StagedImport,
};
pub(crate) use source::{DmabufSourceCache, ImportedDmabufSource};

use smithay::{backend::allocator::dmabuf::Dmabuf, utils::SealedFile};
use std::{
    fs::File,
    io::{Read, Seek},
    os::fd::{AsFd, OwnedFd},
};
use weld_client::{ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, Extent};

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

/// Copies one normalized SHM lease into a sealed descriptor for local transfer.
///
/// This is an explicit compatibility path for clients that submit SHM. DMA-BUF
/// leases must continue through [`export_client_dmabuf`] so their pixels are not
/// copied through CPU memory.
pub fn export_client_shm(lease: &ClientBufferLease) -> anyhow::Result<OwnedFd> {
    use anyhow::{Context, ensure};

    let access = lease
        .access::<DirectClientBufferAccess>()
        .context("client-buffer lease does not contain direct native access")?;
    let DirectClientBufferAccess::Shm(shm) = access else {
        anyhow::bail!("client-buffer lease contains a DMA-BUF, not copied SHM pixels");
    };
    let expected = packed_bgra_len(lease.metadata())?;
    ensure!(
        shm.bgra_pixels.len() == expected,
        "copied SHM pixel length differs from client-buffer metadata"
    );
    let sealed = SealedFile::with_data(c"weld-client-shm", &shm.bgra_pixels)
        .context("failed to seal copied SHM pixels")?;
    sealed
        .as_fd()
        .try_clone_to_owned()
        .context("failed to duplicate sealed SHM descriptor")
}

/// Reads a sealed, tightly packed BGRA descriptor received from a local peer.
pub fn import_client_shm(
    file_descriptor: OwnedFd,
    metadata: ClientBufferMetadata,
) -> anyhow::Result<WaylandShmBuffer> {
    use anyhow::{Context, ensure};
    use smithay::reexports::rustix::fs::{SealFlags, fcntl_get_seals};

    let expected = packed_bgra_len(metadata)?;
    let mut file = File::from(file_descriptor);
    let required_seals = SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE;
    let seals = fcntl_get_seals(&file).context("failed to inspect SHM descriptor seals")?;
    ensure!(
        seals.contains(required_seals),
        "SHM descriptor is not sealed against size and content changes"
    );
    let actual = usize::try_from(
        file.metadata()
            .context("failed to inspect sealed SHM descriptor")?
            .len(),
    )
    .context("sealed SHM descriptor length exceeds address space")?;
    ensure!(
        actual == expected,
        "sealed SHM descriptor length {actual} differs from expected {expected}"
    );
    file.rewind()
        .context("failed to rewind sealed SHM descriptor")?;
    let mut bgra_pixels = vec![0; expected];
    file.read_exact(&mut bgra_pixels)
        .context("failed to read sealed SHM pixels")?;
    Ok(WaylandShmBuffer { bgra_pixels })
}

fn packed_bgra_len(metadata: ClientBufferMetadata) -> anyhow::Result<usize> {
    let pixels = u64::from(metadata.extent.width)
        .checked_mul(u64::from(metadata.extent.height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("SHM pixel length overflow"))?;
    usize::try_from(pixels).map_err(|_| anyhow::anyhow!("SHM pixel length exceeds address space"))
}

/// Native access payload resolved by Weld's built-in application importer.
#[derive(Debug)]
pub enum DirectClientBufferAccess {
    Shm(WaylandShmBuffer),
    Dmabuf(DmabufAccess),
}

/// Marker for adapters whose leases contain [`DirectClientBufferAccess`].
#[derive(Clone, Copy, Debug, Default)]
pub struct DirectClientBufferImporter;

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

    #[test]
    fn copied_shm_roundtrips_through_a_sealed_descriptor() {
        let source = ClientSourceId::new(1);
        let metadata = ClientBufferMetadata::new(Extent::new(2, 1), false);
        let pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let lease = ClientBufferLease::new(
            ClientBufferId::new(source, 2),
            ClientBufferUseId::new(source, 3),
            metadata,
            Rc::new(DirectClientBufferAccess::Shm(WaylandShmBuffer {
                bgra_pixels: pixels.clone(),
            })),
            |_| {},
        )
        .expect("matching source");

        let descriptor = export_client_shm(&lease).expect("sealed SHM export");
        let imported = import_client_shm(descriptor, metadata).expect("sealed SHM import");

        assert_eq!(imported.bgra_pixels, pixels);
    }

    #[test]
    fn sealed_shm_length_must_match_buffer_metadata() {
        let sealed = SealedFile::with_data(c"weld-shm-test", &[0; 4]).expect("sealed test data");
        let descriptor = sealed
            .as_fd()
            .try_clone_to_owned()
            .expect("test descriptor");

        let error = import_client_shm(
            descriptor,
            ClientBufferMetadata::new(Extent::new(2, 1), false),
        )
        .expect_err("mismatched SHM length");

        assert!(error.to_string().contains("differs from expected"));
    }
}
