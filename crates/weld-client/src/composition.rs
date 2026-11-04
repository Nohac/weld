//! Retained input to a source-side, offscreen surface composition.

use crate::{
    ClientBufferLease, ClientCursor, Extent, LogicalPoint, LogicalSize, SurfaceLayerPlacement,
};

/// Back-to-front layers sampled into one opaque, viewport-sized buffer.
/// Buffer leases keep client storage alive through GPU consumption.
#[derive(Clone, Debug)]
pub struct ComposedBuffer {
    pub extent: Extent,
    pub logical_size: LogicalSize,
    pub layers: Vec<CompositionLayer>,
    /// Optional presenter-owned pointer, composed last and independent of client commits.
    pub cursor: Option<CompositionCursor>,
}

#[derive(Clone, Debug)]
pub struct CompositionLayer {
    pub buffer: ClientBufferLease,
    pub placement: SurfaceLayerPlacement,
}

/// Cursor hotspot in the composition's logical coordinate space.
#[derive(Clone, Debug, PartialEq)]
pub struct CompositionCursor {
    pub image: ClientCursor,
    pub position: LogicalPoint,
}
