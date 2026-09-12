//! Native client-import bootstrap, independent of adapter selection and local
//! presentation. Nested selects against its surface; DRM against its device.

use anyhow::Result;
use calloop::channel::{Channel, channel};

use crate::dmabuf::{
    DmabufCapabilities, DmabufContext, DmabufEvent, DmabufSourceCache, request_weld_device,
};

pub(crate) struct NativeGpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub sources: DmabufSourceCache,
    pub capabilities: Option<DmabufCapabilities>,
}

impl NativeGpu {
    pub fn request(adapter: &wgpu::Adapter, label: &'static str) -> Result<Self> {
        let (device, queue, capabilities) = request_weld_device(adapter, label)?;
        let sources = DmabufSourceCache::new(&device);
        Ok(Self {
            device,
            queue,
            sources,
            capabilities,
        })
    }
}

/// The same release channel owns unrendered and renderer-consumed client uses.
/// SHM-only assembly supplies an unavailable source cache, not a fake GPU.
pub(crate) fn import_channel(
    sources: DmabufSourceCache,
    capabilities: Option<DmabufCapabilities>,
) -> (DmabufContext, Channel<DmabufEvent>) {
    let (sender, source) = channel();
    (DmabufContext::new(sender, sources, capabilities), source)
}
