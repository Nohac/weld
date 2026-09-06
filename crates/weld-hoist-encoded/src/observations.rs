//! Bounded source-port diagnostics, independent of the logging filter.
//!
//! These are event counts, not partitions of issued frames or credits: a locally
//! cancelled credit can later produce a stale reply. Normal coalescing and
//! lifecycle cancellation are not congestion drops. Batch wall time includes
//! sequential layer work and host polling; credit turnaround also includes local
//! send queues and remote processing. Neither measures GPU-only time or RTT.

use std::{
    mem,
    time::{Duration, Instant},
};

pub(super) const REPORT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug)]
pub(super) enum SourceObservation {
    CommitReceived,
    CommitCoalesced,
    BatchCompleted {
        layer_frames: usize,
        payload_bytes: u64,
        wall_time: Duration,
    },
    BatchCancelled,
    CodecFailed,
    CreditApplied(Duration),
    CreditCancelled,
    StaleCreditOutcome,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
/// Bounded cumulative timing observations, measured on one local clock.
pub struct TimingSummary {
    /// Number of recorded operations.
    pub samples: u64,
    /// Sum of their durations, saturating on overflow.
    pub total: Duration,
    /// Longest recorded duration.
    pub maximum: Duration,
}

impl TimingSummary {
    /// Record an operation without retaining per-operation history.
    pub fn record(&mut self, duration: Duration) {
        self.samples = self.samples.saturating_add(1);
        self.total = self.total.saturating_add(duration);
        self.maximum = self.maximum.max(duration);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct SourceCounters {
    pub commits_received: u64,
    pub commits_coalesced: u64,
    pub batches_completed: u64,
    pub layer_frames_completed: u64,
    pub encoded_payload_bytes: u64,
    pub batches_cancelled: u64,
    pub codec_failures: u64,
    pub credits_cancelled: u64,
    pub stale_credit_outcomes: u64,
    pub batch_wall: TimingSummary,
    pub applied_credit_turnaround: TimingSummary,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SourceGauges {
    pub pending_events: usize,
    pub awaiting_credit: usize,
    pub active_streams: usize,
    pub encode_in_flight: bool,
    pub oldest_credit_age: Duration,
    pub active_batch_age: Duration,
}

impl SourceGauges {
    fn has_work(self) -> bool {
        self.pending_events != 0 || self.awaiting_credit != 0 || self.encode_in_flight
    }
}

pub(super) struct SourceReport {
    pub elapsed: Duration,
    pub counters: SourceCounters,
    pub gauges: SourceGauges,
    pub final_report: bool,
}

impl SourceReport {
    pub fn emit(self) {
        let Self {
            elapsed,
            counters,
            gauges,
            final_report,
        } = self;
        tracing::debug!(
            target: "weld_media_diag",
            interval_us = elapsed.as_micros(),
            final_report,
            commits_received = counters.commits_received,
            commits_coalesced = counters.commits_coalesced,
            batches_completed = counters.batches_completed,
            layer_frames_completed = counters.layer_frames_completed,
            encoded_payload_bytes = counters.encoded_payload_bytes,
            batches_cancelled = counters.batches_cancelled,
            codec_failures = counters.codec_failures,
            credits_applied = counters.applied_credit_turnaround.samples,
            credits_cancelled = counters.credits_cancelled,
            stale_credit_outcomes = counters.stale_credit_outcomes,
            batch_wall_samples = counters.batch_wall.samples,
            batch_wall_total_us = counters.batch_wall.total.as_micros(),
            batch_wall_max_us = counters.batch_wall.maximum.as_micros(),
            credit_turnaround_total_us = counters.applied_credit_turnaround.total.as_micros(),
            credit_turnaround_max_us = counters.applied_credit_turnaround.maximum.as_micros(),
            pending_events = gauges.pending_events,
            awaiting_credit = gauges.awaiting_credit,
            active_streams = gauges.active_streams,
            encode_in_flight = gauges.encode_in_flight,
            oldest_credit_age_us = gauges.oldest_credit_age.as_micros(),
            active_batch_age_us = gauges.active_batch_age.as_micros(),
            "encoded source observations"
        );
    }
}

/// One fixed-size interval accumulator; taking a report never requests work.
pub(super) struct SourceObservations {
    last_report: Instant,
    counters: SourceCounters,
}

impl SourceObservations {
    pub fn new(now: Instant) -> Self {
        Self {
            last_report: now,
            counters: SourceCounters::default(),
        }
    }

    pub fn record(&mut self, observation: SourceObservation) {
        let counters = &mut self.counters;
        match observation {
            SourceObservation::CommitReceived => {
                counters.commits_received = counters.commits_received.saturating_add(1);
            }
            SourceObservation::CommitCoalesced => {
                counters.commits_coalesced = counters.commits_coalesced.saturating_add(1);
            }
            SourceObservation::BatchCompleted {
                layer_frames,
                payload_bytes,
                wall_time,
            } => {
                counters.batches_completed = counters.batches_completed.saturating_add(1);
                counters.layer_frames_completed = counters
                    .layer_frames_completed
                    .saturating_add(u64::try_from(layer_frames).unwrap_or(u64::MAX));
                counters.encoded_payload_bytes =
                    counters.encoded_payload_bytes.saturating_add(payload_bytes);
                counters.batch_wall.record(wall_time);
            }
            SourceObservation::BatchCancelled => {
                counters.batches_cancelled = counters.batches_cancelled.saturating_add(1);
            }
            SourceObservation::CodecFailed => {
                counters.codec_failures = counters.codec_failures.saturating_add(1);
            }
            SourceObservation::CreditApplied(turnaround) => {
                counters.applied_credit_turnaround.record(turnaround);
            }
            SourceObservation::CreditCancelled => {
                counters.credits_cancelled = counters.credits_cancelled.saturating_add(1);
            }
            SourceObservation::StaleCreditOutcome => {
                counters.stale_credit_outcomes = counters.stale_credit_outcomes.saturating_add(1);
            }
        }
    }

    /// Lets the caller avoid scanning its existing queue maps on every poll.
    pub fn report_due(&self, now: Instant, final_report: bool) -> bool {
        final_report || now.saturating_duration_since(self.last_report) >= REPORT_INTERVAL
    }

    pub fn take_report(
        &mut self,
        now: Instant,
        gauges: SourceGauges,
        final_report: bool,
    ) -> Option<SourceReport> {
        if !self.report_due(now, final_report) {
            return None;
        }
        let elapsed = now.saturating_duration_since(self.last_report);
        let counters = mem::take(&mut self.counters);
        self.last_report = now;
        if counters == SourceCounters::default() && !gauges.has_work() {
            return None;
        }
        Some(SourceReport {
            elapsed,
            counters,
            gauges,
            final_report,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_is_bounded_and_resets_the_interval() {
        let start = Instant::now();
        let mut observations = SourceObservations::new(start);
        observations.record(SourceObservation::CommitReceived);
        assert!(
            observations
                .take_report(
                    start + Duration::from_millis(999),
                    SourceGauges::default(),
                    false
                )
                .is_none()
        );
        let report = observations
            .take_report(start + REPORT_INTERVAL, SourceGauges::default(), false)
            .expect("due report");
        assert_eq!(report.counters.commits_received, 1);
        assert_eq!(report.elapsed, REPORT_INTERVAL);
        assert!(!report.final_report);
        assert!(
            observations
                .take_report(start + REPORT_INTERVAL * 2, SourceGauges::default(), false)
                .is_none()
        );
    }

    #[test]
    fn idle_streams_are_quiet_but_credit_and_pending_work_are_not() {
        let start = Instant::now();
        let mut observations = SourceObservations::new(start);
        let idle = SourceGauges {
            active_streams: 3,
            ..Default::default()
        };
        assert!(
            observations
                .take_report(start + REPORT_INTERVAL, idle, false)
                .is_none()
        );
        let stalled = SourceGauges {
            awaiting_credit: 1,
            oldest_credit_age: Duration::from_secs(2),
            ..idle
        };
        let report = observations
            .take_report(start + REPORT_INTERVAL * 2, stalled, false)
            .expect("stalled report");
        assert_eq!(report.gauges.oldest_credit_age, Duration::from_secs(2));
        assert_eq!(report.counters, SourceCounters::default());
        let pending = SourceGauges {
            pending_events: 1,
            ..idle
        };
        assert!(
            observations
                .take_report(start + REPORT_INTERVAL * 3, pending, false)
                .is_some()
        );
    }

    #[test]
    fn cancellation_and_stale_events_do_not_become_turnaround_samples() {
        let mut observations = SourceObservations::new(Instant::now());
        observations.record(SourceObservation::CreditApplied(Duration::from_millis(40)));
        observations.record(SourceObservation::CreditCancelled);
        observations.record(SourceObservation::StaleCreditOutcome);
        observations.record(SourceObservation::CommitCoalesced);
        let counters = observations.counters;
        assert_eq!(counters.applied_credit_turnaround.samples, 1);
        assert_eq!(
            counters.applied_credit_turnaround.total,
            Duration::from_millis(40)
        );
        // One withdrawn credit can cause both events; they are not disjoint outcomes.
        assert_eq!(counters.credits_cancelled, 1);
        assert_eq!(counters.stale_credit_outcomes, 1);
        assert_eq!(counters.commits_coalesced, 1);
    }

    #[test]
    fn final_partial_report_does_not_require_a_full_interval() {
        let start = Instant::now();
        let mut observations = SourceObservations::new(start);
        observations.record(SourceObservation::CodecFailed);
        let report = observations
            .take_report(start, SourceGauges::default(), true)
            .expect("partial report");
        assert!(report.final_report);
        assert_eq!(report.elapsed, Duration::ZERO);
        assert_eq!(report.counters.codec_failures, 1);
    }

    #[test]
    fn counters_and_duration_totals_saturate() {
        let mut observations = SourceObservations::new(Instant::now());
        observations.counters.commits_received = u64::MAX;
        observations.record(SourceObservation::CommitReceived);
        for _ in 0..2 {
            observations.record(SourceObservation::BatchCompleted {
                layer_frames: 2,
                payload_bytes: u64::MAX,
                wall_time: Duration::MAX,
            });
        }
        assert_eq!(observations.counters.commits_received, u64::MAX);
        assert_eq!(observations.counters.encoded_payload_bytes, u64::MAX);
        assert_eq!(observations.counters.layer_frames_completed, 4);
        assert_eq!(observations.counters.batch_wall.total, Duration::MAX);
        assert_eq!(observations.counters.batch_wall.maximum, Duration::MAX);
    }
}
