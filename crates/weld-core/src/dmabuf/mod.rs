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

/// Smithay-owned access payload resolved by Weld's built-in application importer.
#[derive(Debug)]
pub enum WaylandBufferAccess {
    Shm(WaylandShmBuffer),
    Dmabuf(PendingDmabufFrame),
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

    pub(crate) const fn raw(self) -> u64 {
        self.0
    }

    #[cfg(feature = "test-support")]
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
    GpuUseCompleted(DmabufReleaseId),
    LeaseCompleted(DmabufReleaseId),
}

/// A validated client DMA-BUF crossing from Smithay into the shell renderer.
#[derive(Debug)]
pub struct PendingDmabufFrame {
    dmabuf: Dmabuf,
    release: DmabufReleaseId,
}

impl PendingDmabufFrame {
    pub(crate) fn new(dmabuf: Dmabuf, release: DmabufReleaseId) -> Self {
        Self { dmabuf, release }
    }
}
