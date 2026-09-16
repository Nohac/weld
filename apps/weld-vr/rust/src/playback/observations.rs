//! Session-lifetime presentation counters: frames/checks, not packets or scanouts.
//! Clock-local timings overlap and must not be summed as end-to-end latency.
use super::lock;
use std::{
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Default)]
pub(super) enum Discard {
    Superseded,
    Stale,
    Layout,
    #[default]
    Lifecycle,
    Handoff,
}

#[derive(Default)]
pub(super) struct Observations {
    pub decoded: AtomicU64,
    pub imported: AtomicU64,
    pub render_busy: AtomicU64,
    pub fence_busy: AtomicU64,
    selected: AtomicU64,
    wait_us: AtomicU64,
    wait_max_us: AtomicU64,
    pub commit_gap_max_us: AtomicU64,
    pub commit_bursts: AtomicU64,
    pub import_us: AtomicU64,
    pub import_max_us: AtomicU64,
    pub render_wait_max_us: AtomicU64,
    discarded: [AtomicU64; 5],
    clock: Mutex<Option<(Instant, Instant)>>,
}
impl Observations {
    #[cfg(test)]
    pub fn count(&self, reason: Discard) -> u64 {
        self.discarded[reason as usize].load(Ordering::Relaxed)
    }
    pub fn discard(&self, reason: Discard) {
        self.discarded[reason as usize].fetch_add(1, Ordering::Relaxed);
    }
    pub fn selected(&self, age: Duration) {
        self.selected.fetch_add(1, Ordering::Relaxed);
        self.wait_us.fetch_add(micros(age), Ordering::Relaxed);
        self.wait_max_us.fetch_max(micros(age), Ordering::Relaxed);
    }
    pub fn report(&self, final_report: bool) {
        let now = Instant::now();
        let mut clock = lock(&self.clock);
        let (start, previous) = clock.get_or_insert((now, now));
        if !final_report && now.duration_since(*previous) < Duration::from_secs(1) {
            return;
        }
        *previous = now;
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        tracing::debug!(target: "weld_vr_diag",
            elapsed_us = micros(now.duration_since(*start)), final_report,
            decoded_total = load(&self.decoded), imported_total = load(&self.imported),
            superseded_total = load(&self.discarded[0]), stale_total = load(&self.discarded[1]),
            layout_discard_total = load(&self.discarded[2]), lifecycle_discard_total = load(&self.discarded[3]),
            handoff_discard_total = load(&self.discarded[4]),
            render_busy_checks_total = load(&self.render_busy), fence_busy_checks_total = load(&self.fence_busy),
            selected_snapshots_total = load(&self.selected), snapshot_wait_total_us = load(&self.wait_us),
            snapshot_wait_max_us = self.wait_max_us.swap(0, Ordering::Relaxed),
            commit_gap_max_us = self.commit_gap_max_us.swap(0, Ordering::Relaxed),
            commit_bursts_total = load(&self.commit_bursts),
            import_cpu_total_us = load(&self.import_us),
            import_cpu_max_us = self.import_max_us.swap(0, Ordering::Relaxed),
            render_handoff_max_us = self.render_wait_max_us.swap(0, Ordering::Relaxed),
            "presentation observations");
    }
}
pub(super) fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lifecycle_and_staleness_never_count_as_supersession_or_network_loss() {
        let stats = Observations::default();
        stats.discard(Discard::Lifecycle);
        stats.discard(Discard::Stale);
        stats.discard(Discard::Stale);
        assert_eq!(stats.discarded.map(|v| v.into_inner()), [0, 2, 0, 1, 0]);
    }
}
