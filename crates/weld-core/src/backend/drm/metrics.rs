use std::{
    ffi::OsStr,
    time::{Duration, Instant},
};

use tracing::info;

use super::presenter::CursorUpdateOutcome;

const METRICS_ENVIRONMENT_VARIABLE: &str = "WELD_DRM_METRICS";
const REPORT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct IntervalCounters {
    loop_iterations: u64,
    input_batches: u64,
    raw_input_events: u64,
    forwarded_client_events: u64,
    surface_events: u64,
    surface_commits: u64,
    main_updates: u64,
    redraw_promotions: u64,
    compositions: u64,
    presentation_batches: u64,
    output_presentations: u64,
    cursor_motion_refreshes: u64,
    cursor_motion_compositions: u64,
    cursor_sync_refreshes: u64,
    cursor_sync_compositions: u64,
    cursor_vblank_retirements: u64,
    cursor_vblank_compositions: u64,
    hardware_cursor_commits: u64,
    hardware_cursor_fallbacks: u64,
}

impl IntervalCounters {
    const fn has_activity(&self) -> bool {
        self.loop_iterations > 0
    }

    fn record_cursor_outcome(&mut self, outcome: CursorUpdateOutcome) {
        self.hardware_cursor_commits += outcome.hardware_commits;
        self.hardware_cursor_fallbacks += outcome.fallback_activations;
    }
}

pub(super) struct DrmRuntimeMetrics {
    enabled: bool,
    interval_started_at: Instant,
    counters: IntervalCounters,
}

impl DrmRuntimeMetrics {
    pub(super) fn from_environment(now: Instant) -> Self {
        Self {
            enabled: metrics_enabled(std::env::var_os(METRICS_ENVIRONMENT_VARIABLE).as_deref()),
            interval_started_at: now,
            counters: IntervalCounters::default(),
        }
    }

    pub(super) fn record_loop_iteration(&mut self) {
        if self.enabled {
            self.counters.loop_iterations += 1;
        }
    }

    pub(super) fn record_input_batch(&mut self, raw_events: usize, forwarded_events: usize) {
        if !self.enabled || raw_events == 0 {
            return;
        }
        self.counters.input_batches += 1;
        self.counters.raw_input_events += raw_events as u64;
        self.counters.forwarded_client_events += forwarded_events as u64;
    }

    pub(super) fn record_surface_batch(&mut self, events: usize, commits: usize) {
        if !self.enabled {
            return;
        }
        self.counters.surface_events += events as u64;
        self.counters.surface_commits += commits as u64;
    }

    pub(super) fn record_main_update(&mut self, requested_redraw: bool) {
        if !self.enabled {
            return;
        }
        self.counters.main_updates += 1;
        self.counters.redraw_promotions += u64::from(requested_redraw);
    }

    pub(super) fn record_composition(&mut self) {
        if self.enabled {
            self.counters.compositions += 1;
        }
    }

    pub(super) fn record_presentation(&mut self, outputs: usize) {
        if !self.enabled {
            return;
        }
        self.counters.presentation_batches += 1;
        self.counters.output_presentations += outputs as u64;
    }

    pub(super) fn record_motion_cursor_refresh(&mut self, outcome: CursorUpdateOutcome) {
        if !self.enabled {
            return;
        }
        self.counters.cursor_motion_refreshes += 1;
        self.counters.cursor_motion_compositions += u64::from(outcome.composition_required);
        self.counters.record_cursor_outcome(outcome);
    }

    pub(super) fn record_sync_cursor_refresh(&mut self, outcome: CursorUpdateOutcome) {
        if !self.enabled {
            return;
        }
        self.counters.cursor_sync_refreshes += 1;
        self.counters.cursor_sync_compositions += u64::from(outcome.composition_required);
        self.counters.record_cursor_outcome(outcome);
    }

    pub(super) fn record_vblank_cursor_retirement(&mut self, outcome: CursorUpdateOutcome) {
        if !self.enabled {
            return;
        }
        self.counters.cursor_vblank_retirements += outcome.vblank_retirements;
        self.counters.cursor_vblank_compositions += u64::from(outcome.composition_required);
        self.counters.record_cursor_outcome(outcome);
    }

    pub(super) fn record_presenter_cursor_outcome(&mut self, outcome: CursorUpdateOutcome) {
        if !self.enabled {
            return;
        }
        self.counters.record_cursor_outcome(outcome);
    }

    pub(super) fn report_if_due(&mut self, now: Instant) {
        if self.enabled && now.duration_since(self.interval_started_at) >= REPORT_INTERVAL {
            self.report(now, false);
        }
    }

    fn report(&mut self, now: Instant, final_interval: bool) {
        let elapsed = now.duration_since(self.interval_started_at);
        if !self.counters.has_activity() {
            self.interval_started_at = now;
            return;
        }
        let counters = std::mem::take(&mut self.counters);
        self.interval_started_at = now;
        info!(
            target: "weld_metrics",
            interval_ms = elapsed.as_millis() as u64,
            final_interval,
            loop_iterations = counters.loop_iterations,
            input_batches = counters.input_batches,
            raw_input_events = counters.raw_input_events,
            forwarded_client_events = counters.forwarded_client_events,
            surface_events = counters.surface_events,
            surface_commits = counters.surface_commits,
            main_updates = counters.main_updates,
            redraw_promotions = counters.redraw_promotions,
            compositions = counters.compositions,
            presentation_batches = counters.presentation_batches,
            output_presentations = counters.output_presentations,
            cursor_motion_refreshes = counters.cursor_motion_refreshes,
            cursor_motion_compositions = counters.cursor_motion_compositions,
            cursor_sync_refreshes = counters.cursor_sync_refreshes,
            cursor_sync_compositions = counters.cursor_sync_compositions,
            cursor_vblank_retirements = counters.cursor_vblank_retirements,
            cursor_vblank_compositions = counters.cursor_vblank_compositions,
            hardware_cursor_commits = counters.hardware_cursor_commits,
            hardware_cursor_fallbacks = counters.hardware_cursor_fallbacks,
            "DRM runtime metrics"
        );
    }
}

impl Drop for DrmRuntimeMetrics {
    fn drop(&mut self) {
        if self.enabled {
            self.report(Instant::now(), true);
        }
    }
}

fn metrics_enabled(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != OsStr::new("0"))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::metrics_enabled;

    #[test]
    fn metrics_require_an_explicit_nonzero_environment_value() {
        assert!(!metrics_enabled(None));
        assert!(!metrics_enabled(Some(OsStr::new(""))));
        assert!(!metrics_enabled(Some(OsStr::new("0"))));
        assert!(metrics_enabled(Some(OsStr::new("1"))));
    }
}
