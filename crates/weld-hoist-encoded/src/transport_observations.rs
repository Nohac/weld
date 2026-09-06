//! Owned binding measurements. No transport implementation or remote clocks.

use std::time::{Duration, Instant};

use crate::TimingSummary;

/// Lifetime totals for one connection's encoded media send path.
///
/// Payload bytes exclude framing/transport overhead. Completed writes mean
/// acceptance by the transport API, not remote receipt, decoding or display.
/// Cancelled records include teardown and write errors, not congestion drops.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MediaSendCounters {
    /// Records admitted to the binding's media queue.
    pub accepted_records: u64,
    /// Payload sizes of admitted records, excluding framing.
    pub accepted_payload_bytes: u64,
    /// Records whose complete framed write succeeded.
    pub completed_records: u64,
    /// Payload sizes of successfully written records.
    pub completed_payload_bytes: u64,
    /// Admitted records dropped without completing their full write.
    pub cancelled_records: u64,
    /// Full payload sizes of cancelled records, including partially written ones.
    pub cancelled_payload_bytes: u64,
    /// Admission to beginning the full framed write, including task scheduling.
    pub queue_wait: TimingSummary,
    /// Successful full framed write duration, including waiting and scheduling.
    pub write_wall: TimingSummary,
}

/// Current application-owned send backlog, including the active full record.
/// This is not the transport's count of unsent or unacknowledged wire bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct MediaSendSnapshot {
    /// Connection-lifetime totals; do not sum successive snapshots.
    pub counters: MediaSendCounters,
    /// Queued records plus any active framed write.
    pub pending_records: usize,
    /// Full payload sizes of all pending records.
    pub pending_payload_bytes: u64,
    /// Age since admission of the oldest pending record.
    pub oldest_pending_age: Duration,
    /// Age of the active write, or None when no write is active.
    pub active_write_age: Option<Duration>,
}

/// Selected-path measurements scoped to one transport connection.
///
/// Rebaseline cumulative counter deltas whenever `epoch` changes. RTT and cwnd
/// are hints, not available bandwidth. Packet loss is not lost video frames.
/// A stale snapshot is not evidence that a path remains live or healthy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetworkPathSnapshot {
    /// Local monotonic time of the path sample, not a remote timestamp.
    pub sampled_at: Instant,
    /// Local selection epoch, changed on selection changes or uncertainty.
    pub epoch: u64,
    /// Transport's current round-trip-time estimate.
    pub rtt: Duration,
    /// Transport's current congestion window, not an application allowance.
    pub congestion_window_bytes: u64,
    /// Path's cumulative transport-byte count, including non-media traffic.
    pub sent_bytes: u64,
    /// Path's cumulative received transport bytes.
    pub received_bytes: u64,
    /// Path's cumulative transport packet losses, not video-frame losses.
    pub lost_packets: u64,
    /// Path's cumulative transport bytes declared lost.
    pub lost_bytes: u64,
    /// Path's cumulative transport congestion events.
    pub congestion_events: u64,
}

/// One owned observation of a binding's send path.
#[derive(Clone, Copy, Debug)]
pub struct TransportSnapshot {
    /// Local instant used to calculate send-backlog ages.
    pub observed_at: Instant,
    /// Current backlog and lifetime totals for the encoded send path.
    pub media: MediaSendSnapshot,
    /// None means no usable selected-path observation, not a healthy local link.
    pub path: Option<NetworkPathSnapshot>,
}

impl TransportSnapshot {
    pub(super) fn emit(self) {
        let media = self.media;
        let counters = media.counters;
        tracing::debug!(target: "weld_media_diag",
            accepted_records_total = counters.accepted_records,
            accepted_payload_bytes_total = counters.accepted_payload_bytes,
            completed_records_total = counters.completed_records,
            completed_payload_bytes_total = counters.completed_payload_bytes,
            cancelled_records_total = counters.cancelled_records,
            cancelled_payload_bytes_total = counters.cancelled_payload_bytes,
            queue_wait_samples = counters.queue_wait.samples,
            queue_wait_total_us = counters.queue_wait.total.as_micros(),
            queue_wait_max_us = counters.queue_wait.maximum.as_micros(),
            write_wall_samples = counters.write_wall.samples,
            write_wall_total_us = counters.write_wall.total.as_micros(),
            write_wall_max_us = counters.write_wall.maximum.as_micros(),
            pending_records = media.pending_records,
            pending_payload_bytes = media.pending_payload_bytes,
            oldest_pending_age_us = media.oldest_pending_age.as_micros(),
            active_write_age_us = ?media.active_write_age.map(|age| age.as_micros()),
            path_available = self.path.is_some(),
            "encoded transport observations");
        if let Some(path) = self.path {
            tracing::debug!(target: "weld_media_diag",
                path_epoch = path.epoch,
                sample_age_us = self.observed_at.saturating_duration_since(path.sampled_at).as_micros(),
                rtt_us = path.rtt.as_micros(),
                congestion_window_bytes = path.congestion_window_bytes,
                sent_bytes_total = path.sent_bytes,
                received_bytes_total = path.received_bytes,
                lost_packets_total = path.lost_packets,
                lost_bytes_total = path.lost_bytes,
                congestion_events_total = path.congestion_events,
                "encoded selected-path observations");
        }
    }
}
