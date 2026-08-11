//! Linux DMA-BUF capability discovery and GPU import.

mod device;
mod importer;
mod source;

pub(crate) use device::{DmabufCapabilities, request_weld_device};
pub(crate) use importer::DmabufImporter;
pub(crate) use source::{DmabufSourceCache, ImportedDmabufSource};

use smithay::backend::allocator::dmabuf::Dmabuf;

/// Server-owned identity returned only after GPU consumption has completed.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DmabufReleaseId(u64);

impl DmabufReleaseId {
    pub(crate) const fn new(raw: u64) -> Self {
        Self(raw)
    }
}

/// A validated client DMA-BUF crossing from Smithay into the shell renderer.
#[derive(Debug)]
pub(crate) struct PendingDmabufFrame {
    pub(crate) dmabuf: Dmabuf,
    pub(crate) release: DmabufReleaseId,
}
