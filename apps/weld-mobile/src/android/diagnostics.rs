//! Bounded observations between decoded publication and GPU conversion.
use super::receiver::Shared;
use std::time::{Duration, Instant};
use weld_hoist_encoded::TimingSummary;

#[derive(Default)]
pub(super) struct PresentationDiagnostics {
    since: Option<Instant>,
    published: u64,
    superseded: u64,
    stale: u64,
    invalidated: u64,
    frames: u64,
    age: TimingSummary,
    conversion: TimingSummary,
}

impl PresentationDiagnostics {
    pub fn record(&mut self, age: Duration, conversion: Duration) {
        self.frames += 1;
        self.age.record(age);
        self.conversion.record(conversion);
    }

    pub fn report(&mut self, shared: &Shared) {
        let now = Instant::now();
        let since = *self.since.get_or_insert(now);
        let elapsed = now.duration_since(since);
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let Some(stats) = shared.latest.lock().ok().map(|queue| queue.stats()) else {
            return;
        };
        let published = stats.submitted;
        let superseded = stats.superseded;
        let stale = stats.stale;
        let invalidated = stats.invalidated;
        if let Some(recorder) = shared.reports.recorder() {
            recorder.record(weld_diagnostics::Observation::Stage {
                stage: weld_diagnostics::Stage::Presentation,
                sample: weld_diagnostics::StageSample {
                    interval_us: weld_diagnostics::micros(elapsed),
                    completed: self.frames,
                    work_max_us: weld_diagnostics::micros(self.conversion.maximum),
                    superseded: superseded.saturating_sub(self.superseded),
                    stale: stale.saturating_sub(self.stale),
                    ..Default::default()
                },
            });
        }
        if self.frames > 0 || published != self.published {
            tracing::info!(target: "weld_mobile_diag",
                interval_us = elapsed.as_micros(),
                published = published.saturating_sub(self.published),
                superseded = superseded.saturating_sub(self.superseded),
                stale = stale.saturating_sub(self.stale),
                invalidated = invalidated.saturating_sub(self.invalidated),
                presented = self.frames,
                handoff_total_us = self.age.total.as_micros(),
                handoff_max_us = self.age.maximum.as_micros(),
                conversion_total_us = self.conversion.total.as_micros(),
                conversion_max_us = self.conversion.maximum.as_micros(),
                "phone presentation observations");
        }
        self.since = Some(now);
        self.published = published;
        self.superseded = superseded;
        self.stale = stale;
        self.invalidated = invalidated;
        self.frames = 0;
        self.age = TimingSummary::default();
        self.conversion = TimingSummary::default();
    }
}
