//! Application redraw requests to custom-widget paint entry, on one local clock.
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Wake {
    first: Option<Instant>,
}
impl Wake {
    pub fn request(&mut self, now: Instant) {
        self.first.get_or_insert(now);
    }
    pub fn take(&mut self) -> Option<Instant> {
        self.first.take()
    }
}

#[derive(Default)]
pub struct PaintTiming {
    last: Option<Instant>,
    paints: u64,
    requests: u64,
    wait: Duration,
    max_wait: Duration,
    intervals: u64,
    interval: Duration,
    buckets: [u64; 4],
}
impl PaintTiming {
    pub fn paint(&mut self, now: Instant, requested: Option<Instant>) {
        self.paints += 1;
        if let Some(requested) = requested {
            let wait = now.saturating_duration_since(requested);
            self.requests += 1;
            self.wait += wait;
            self.max_wait = self.max_wait.max(wait);
        }
        if let Some(last) = self.last.replace(now) {
            let interval = now.saturating_duration_since(last);
            self.intervals += 1;
            self.interval += interval;
            let bucket = if interval <= Duration::from_millis(10) {
                0
            } else if interval <= Duration::from_millis(20) {
                1
            } else if interval <= Duration::from_millis(30) {
                2
            } else {
                3
            };
            self.buckets[bucket] += 1;
        }
    }
    pub fn report(&mut self, elapsed: Duration) {
        log::info!(
            "probe_paint elapsed_ms={} paints={} wake_samples={} wake_total_us={} wake_max_us={} interval_samples={} interval_total_us={} gap_le10ms={} gap_le20ms={} gap_le30ms={} gap_gt30ms={}",
            elapsed.as_millis(),
            self.paints,
            self.requests,
            self.wait.as_micros(),
            self.max_wait.as_micros(),
            self.intervals,
            self.interval.as_micros(),
            self.buckets[0],
            self.buckets[1],
            self.buckets[2],
            self.buckets[3]
        );
        *self = Self {
            last: self.last,
            ..Self::default()
        };
    }
    pub fn suspend(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coalesced_requests_keep_the_oldest_unserviced_time() {
        let start = Instant::now();
        let mut wake = Wake::default();
        wake.request(start);
        wake.request(start + Duration::from_millis(4));
        assert_eq!(wake.take(), Some(start));
        assert_eq!(wake.take(), None);
        wake.request(start + Duration::from_millis(6));
        assert_eq!(wake.take(), Some(start + Duration::from_millis(6)));
    }
    #[test]
    fn paint_intervals_and_request_age_have_separate_counts() {
        let start = Instant::now();
        let mut timing = PaintTiming::default();
        timing.paint(start, None);
        timing.paint(
            start + Duration::from_millis(17),
            Some(start + Duration::from_millis(5)),
        );
        assert_eq!(timing.paints, 2);
        assert_eq!(timing.requests, 1);
        assert_eq!(timing.wait, Duration::from_millis(12));
        assert_eq!(timing.buckets, [0, 1, 0, 0]);
    }
}
