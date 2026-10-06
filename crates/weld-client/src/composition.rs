//! Retained input to a source-side, offscreen surface composition.

use crate::{ClientBufferLease, Extent, LogicalSize, SurfaceLayerPlacement};

/// Back-to-front layers sampled into one opaque, viewport-sized buffer.
/// Buffer leases keep client storage alive through GPU consumption.
#[derive(Clone, Debug)]
pub struct ComposedBuffer {
    pub extent: Extent,
    pub logical_size: LogicalSize,
    pub layers: Vec<CompositionLayer>,
}

#[derive(Clone, Debug)]
pub struct CompositionLayer {
    pub buffer: ClientBufferLease,
    pub placement: SurfaceLayerPlacement,
}
