//! Local worker timing. These monotonic timestamps are never serialized.

use std::time::Instant;

/// Job residence boundaries on one receiver's clock, not network or GPU-only time.
#[derive(Clone, Copy, Debug)]
pub struct DecodeTiming {
    pub queued_at: Instant,
    pub started_at: Instant,
    pub completed_at: Instant,
    pub pipeline: Option<DecodePipelineTiming>,
}

/// A submission can overlap an older frame's decode or conversion. Intervals
/// for different jobs overlap and must not be summed as CPU/GPU utilization.
#[derive(Clone, Copy, Debug)]
pub struct DecodePipelineTiming {
    pub submitted_at: Instant,
    pub finishing_at: Instant,
    pub had_pending_frame: bool,
}
