//! Linux DMA-BUF capability discovery and GPU import.

mod device;
mod manager;
mod source;

pub use device::{DmabufCapabilities, request_weld_device};
pub use manager::{
    DmabufContext, DmabufManager, ImportId, ImportedImageRegistry, PromotionImage, StagedImport,
};
pub(crate) use source::{DmabufSourceCache, ImportedDmabufSource};

use smithay::backend::allocator::dmabuf::Dmabuf;

/// Server-owned identity returned only after GPU consumption has completed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DmabufReleaseId(u64);

impl DmabufReleaseId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
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
