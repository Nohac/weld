//! Local worker timing. These monotonic timestamps are never serialized.

use std::time::Instant;

/// Execution boundaries on one receiver's clock, not network or GPU-only time.
#[derive(Clone, Copy, Debug)]
pub struct DecodeTiming {
    pub queued_at: Instant,
    pub started_at: Instant,
    pub completed_at: Instant,
}
