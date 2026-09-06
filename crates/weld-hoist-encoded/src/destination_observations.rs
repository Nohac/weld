//! Bounded receiver measurements collected independently from tracing.
//!
//! Ingress is entry into the encoded port, not socket receipt. `media_wait`
//! includes waiting for control metadata and earlier work. `decode_wall` includes
//! worker processing and host completion polling. `commit_wall` runs from control
//! ingress through import and queuing the adapter event, not presentation. These
//! intervals overlap and must not be added or treated as network/GPU-only time.
//! Lifecycle cancellation and late cancelled media are not congestion drops.

use std::{
    mem,
    time::{Duration, Instant},
};

use crate::observations::{REPORT_INTERVAL, TimingSummary};

#[derive(Clone, Copy, Debug)]
pub(super) enum DestinationObservation {
    CommitReceived,
    MediaReceived { payload_bytes: u64 },
    DecodeSubmitted { media_wait: Duration },
    DecodeCompleted { wall_time: Duration },
    DecodeCancelled,
    CodecFailed,
    CommitApplied { wall_time: Duration },
    CommitCancelled,
    LateCancelledMedia,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct DestinationCounters {
    pub commits_received: u64,
    pub media_received: u64,
    pub payload_bytes_received: u64,
    pub decodes_cancelled: u64,
    pub codec_failures: u64,
    pub commits_cancelled: u64,
    pub late_cancelled_media: u64,
    pub media_wait: TimingSummary,
    pub decode_wall: TimingSummary,
    pub commit_wall: TimingSummary,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct DestinationGauges {
    pub pending_events: usize,
    pub pending_media_frames: usize,
    pub pending_media_bytes: u64,
    pub decoded_frames: usize,
    pub active_streams: usize,
    pub decode_in_flight: bool,
    pub oldest_control_age: Duration,
    pub oldest_media_age: Duration,
    pub active_decode_age: Duration,
}

impl DestinationGauges {
    fn has_work(self) -> bool {
        self.pending_events != 0
            || self.pending_media_frames != 0
            || self.decoded_frames != 0
            || self.decode_in_flight
    }
}

pub(super) struct DestinationReport {
    pub elapsed: Duration,
    pub counters: DestinationCounters,
    pub gauges: DestinationGauges,
    pub final_report: bool,
}

impl DestinationReport {
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
            media_received = counters.media_received,
            payload_bytes_received = counters.payload_bytes_received,
            decodes_submitted = counters.media_wait.samples,
            decodes_completed = counters.decode_wall.samples,
            decodes_cancelled = counters.decodes_cancelled,
            codec_failures = counters.codec_failures,
            commits_applied = counters.commit_wall.samples,
            commits_cancelled = counters.commits_cancelled,
            late_cancelled_media = counters.late_cancelled_media,
            media_wait_total_us = counters.media_wait.total.as_micros(),
            media_wait_max_us = counters.media_wait.maximum.as_micros(),
            decode_wall_total_us = counters.decode_wall.total.as_micros(),
            decode_wall_max_us = counters.decode_wall.maximum.as_micros(),
            commit_wall_total_us = counters.commit_wall.total.as_micros(),
            commit_wall_max_us = counters.commit_wall.maximum.as_micros(),
            pending_events = gauges.pending_events,
            pending_media_frames = gauges.pending_media_frames,
            pending_media_bytes = gauges.pending_media_bytes,
            decoded_frames = gauges.decoded_frames,
            active_streams = gauges.active_streams,
            decode_in_flight = gauges.decode_in_flight,
            oldest_control_age_us = gauges.oldest_control_age.as_micros(),
            oldest_media_age_us = gauges.oldest_media_age.as_micros(),
            active_decode_age_us = gauges.active_decode_age.as_micros(),
            "encoded destination observations"
        );
    }
}

/// Constant-size interval state; no history, timer, or scheduling side effects.
pub(super) struct DestinationObservations {
    last_report: Instant,
    counters: DestinationCounters,
}

impl DestinationObservations {
    pub fn new(now: Instant) -> Self {
        Self {
            last_report: now,
            counters: DestinationCounters::default(),
        }
    }

    pub fn record(&mut self, observation: DestinationObservation) {
        let counters = &mut self.counters;
        match observation {
            DestinationObservation::CommitReceived => {
                counters.commits_received = counters.commits_received.saturating_add(1);
            }
            DestinationObservation::MediaReceived { payload_bytes } => {
                counters.media_received = counters.media_received.saturating_add(1);
                counters.payload_bytes_received = counters
                    .payload_bytes_received
                    .saturating_add(payload_bytes);
            }
            DestinationObservation::DecodeSubmitted { media_wait } => {
                counters.media_wait.record(media_wait)
            }
            DestinationObservation::DecodeCompleted { wall_time } => {
                counters.decode_wall.record(wall_time)
            }
            DestinationObservation::DecodeCancelled => {
                counters.decodes_cancelled = counters.decodes_cancelled.saturating_add(1);
            }
            DestinationObservation::CodecFailed => {
                counters.codec_failures = counters.codec_failures.saturating_add(1);
            }
            DestinationObservation::CommitApplied { wall_time } => {
                counters.commit_wall.record(wall_time)
            }
            DestinationObservation::CommitCancelled => {
                counters.commits_cancelled = counters.commits_cancelled.saturating_add(1);
            }
            DestinationObservation::LateCancelledMedia => {
                counters.late_cancelled_media = counters.late_cancelled_media.saturating_add(1);
            }
        }
    }

    /// Avoid scans of the existing queue maps on every poll.
    pub fn report_due(&self, now: Instant, final_report: bool) -> bool {
        final_report || now.saturating_duration_since(self.last_report) >= REPORT_INTERVAL
    }

    pub fn take_report(
        &mut self,
        now: Instant,
        gauges: DestinationGauges,
        final_report: bool,
    ) -> Option<DestinationReport> {
        if !self.report_due(now, final_report) {
            return None;
        }
        let elapsed = now.saturating_duration_since(self.last_report);
        let counters = mem::take(&mut self.counters);
        self.last_report = now;
        // Quiet intervals still advance the window; there are no counters to lose.
        if counters == DestinationCounters::default() && !gauges.has_work() {
            return None;
        }
        Some(DestinationReport {
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
    fn report_respects_interval_and_resets_without_a_logging_subscriber() {
        let start = Instant::now();
        let mut observations = DestinationObservations::new(start);
        observations.record(DestinationObservation::CommitReceived);
        observations.record(DestinationObservation::MediaReceived { payload_bytes: 321 });
        assert!(!observations.report_due(start + REPORT_INTERVAL - Duration::from_nanos(1), false));
        assert!(
            observations
                .take_report(start, DestinationGauges::default(), false)
                .is_none()
        );
        let report = observations
            .take_report(start + REPORT_INTERVAL, DestinationGauges::default(), false)
            .expect("due report");
        assert_eq!(report.elapsed, REPORT_INTERVAL);
        assert!(!report.final_report);
        assert_eq!(report.counters.commits_received, 1);
        assert_eq!(report.counters.media_received, 1);
        assert_eq!(report.counters.payload_bytes_received, 321);
        assert!(
            observations
                .take_report(
                    start + REPORT_INTERVAL * 2,
                    DestinationGauges::default(),
                    false
                )
                .is_none()
        );
    }

    #[test]
    fn idle_streams_are_quiet_but_each_pending_stage_is_reported() {
        let start = Instant::now();
        let mut observations = DestinationObservations::new(start);
        let idle = DestinationGauges {
            active_streams: 3,
            ..Default::default()
        };
        assert!(observations.take_report(start, idle, true).is_none());
        for gauges in [
            DestinationGauges {
                pending_events: 1,
                ..idle
            },
            DestinationGauges {
                pending_media_frames: 1,
                ..idle
            },
            DestinationGauges {
                decoded_frames: 1,
                ..idle
            },
            DestinationGauges {
                decode_in_flight: true,
                active_decode_age: Duration::from_secs(4),
                ..idle
            },
        ] {
            let report = observations
                .take_report(start, gauges, true)
                .expect("pending stage");
            assert_eq!(report.counters, DestinationCounters::default());
            assert_eq!(report.gauges.active_decode_age, gauges.active_decode_age);
            assert!(report.final_report);
        }
    }

    #[test]
    fn overlapping_timings_and_cancellation_are_separate_observations() {
        let start = Instant::now();
        let mut observations = DestinationObservations::new(start);
        observations.record(DestinationObservation::DecodeSubmitted {
            media_wait: Duration::from_millis(10),
        });
        observations.record(DestinationObservation::DecodeCompleted {
            wall_time: Duration::from_millis(20),
        });
        observations.record(DestinationObservation::CommitApplied {
            wall_time: Duration::from_millis(40),
        });
        observations.record(DestinationObservation::DecodeCancelled);
        observations.record(DestinationObservation::CodecFailed);
        observations.record(DestinationObservation::CommitCancelled);
        observations.record(DestinationObservation::LateCancelledMedia);
        let report = observations
            .take_report(start, DestinationGauges::default(), true)
            .expect("partial report");
        assert_eq!(report.counters.media_wait.total, Duration::from_millis(10));
        assert_eq!(report.counters.decode_wall.total, Duration::from_millis(20));
        assert_eq!(report.counters.commit_wall.total, Duration::from_millis(40));
        assert_eq!(report.counters.media_wait.samples, 1);
        assert_eq!(report.counters.decode_wall.samples, 1);
        assert_eq!(report.counters.commit_wall.samples, 1);
        // An obsolete failed job counts both cancellation and failure, not success.
        assert_eq!(report.counters.decodes_cancelled, 1);
        assert_eq!(report.counters.codec_failures, 1);
        assert_eq!(report.counters.commits_cancelled, 1);
        assert_eq!(report.counters.late_cancelled_media, 1);
    }

    #[test]
    fn counters_saturate_and_future_clock_does_not_underflow() {
        let start = Instant::now();
        let mut observations = DestinationObservations::new(start + REPORT_INTERVAL);
        observations.counters.commits_cancelled = u64::MAX;
        observations.record(DestinationObservation::CommitCancelled);
        for _ in 0..2 {
            observations.record(DestinationObservation::MediaReceived {
                payload_bytes: u64::MAX,
            });
            observations.record(DestinationObservation::DecodeCompleted {
                wall_time: Duration::MAX,
            });
        }
        let report = observations
            .take_report(start, DestinationGauges::default(), true)
            .expect("partial report");
        assert_eq!(report.elapsed, Duration::ZERO);
        assert_eq!(report.counters.commits_cancelled, u64::MAX);
        assert_eq!(report.counters.payload_bytes_received, u64::MAX);
        assert_eq!(report.counters.decode_wall.total, Duration::MAX);
        assert_eq!(report.counters.decode_wall.maximum, Duration::MAX);
    }
}
