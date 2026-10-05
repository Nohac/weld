//! Bounded frame handoff shared by desktop, phone, and XR presenters.

use std::time::{Duration, Instant};

/// Snapshot counts at one handoff stage; these do not measure network loss
/// or physical display scanout. Each submitted item has one terminal outcome.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PresentationQueueStats {
    pub submitted: u64,
    pub selected: u64,
    pub superseded: u64,
    pub stale: u64,
    pub invalidated: u64,
}

impl std::ops::AddAssign for PresentationQueueStats {
    fn add_assign(&mut self, other: Self) {
        self.submitted = self.submitted.saturating_add(other.submitted);
        self.selected = self.selected.saturating_add(other.selected);
        self.superseded = self.superseded.saturating_add(other.superseded);
        self.stale = self.stale.saturating_add(other.stale);
        self.invalidated = self.invalidated.saturating_add(other.invalidated);
    }
}

/// Frame selection at display opportunities. Smoothing retains one extra
/// snapshot to absorb arrival jitter, with a refresh-derived age ceiling.
/// Evictions are returned so native resources can be released outside locks.
pub struct PresentationMailbox<T> {
    first: Option<(Instant, T)>,
    second: Option<(Instant, T)>,
    maximum_age: Option<Duration>,
    stats: PresentationQueueStats,
}
impl<T> Default for PresentationMailbox<T> {
    fn default() -> Self {
        Self {
            first: None,
            second: None,
            maximum_age: None,
            stats: PresentationQueueStats::default(),
        }
    }
}
impl<T> PresentationMailbox<T> {
    /// Keep two snapshots, permitting at most two display intervals of history.
    pub fn smoothing(interval: Duration) -> Self {
        Self {
            maximum_age: Some(Self::age_limit(interval)),
            ..Self::default()
        }
    }
    fn age_limit(interval: Duration) -> Duration {
        interval.saturating_mul(2).min(Duration::from_millis(34))
    }
    pub fn stats(&self) -> PresentationQueueStats {
        self.stats
    }
    pub(crate) fn enable_smoothing(&mut self, interval: Duration) {
        self.maximum_age = Some(Self::age_limit(interval));
    }
    pub fn is_empty(&self) -> bool {
        self.first.is_none()
    }
    pub fn newest(&self) -> Option<&T> {
        self.second
            .as_ref()
            .or(self.first.as_ref())
            .map(|(_, item)| item)
    }
    pub fn newest_mut(&mut self) -> Option<&mut T> {
        self.second
            .as_mut()
            .or(self.first.as_mut())
            .map(|(_, item)| item)
    }
    /// Return the oldest superseded snapshot when the bounded handoff is full.
    pub fn push(&mut self, item: T, now: Instant) -> Option<T> {
        self.stats.submitted += 1;
        if self.first.is_none() || self.maximum_age.is_none() {
            self.stats.superseded += u64::from(self.first.is_some());
            return self.first.replace((now, item)).map(|(_, item)| item);
        }
        if self.second.is_none() {
            self.second = Some((now, item));
            return None;
        }
        self.stats.superseded += 1;
        let removed = self.first.take().map(|(_, item)| item);
        self.first = self.second.replace((now, item));
        removed
    }
    pub fn set_interval(&mut self, interval: Duration) {
        if self.maximum_age.is_some() {
            self.maximum_age = Some(Self::age_limit(interval));
        }
    }
    /// Collapse history before an ordered control, preserving the newest age.
    pub fn keep_latest(&mut self) -> Option<T> {
        let newest = self.second.take()?;
        self.stats.invalidated += 1;
        self.first.replace(newest).map(|(_, item)| item)
    }
    pub fn reset(&mut self, item: T) {
        self.clear();
        self.push(item, Instant::now());
    }
    pub fn clear(&mut self) {
        drop(self.drain());
    }
    /// Transfer ownership before releasing a synchronization guard.
    pub fn drain(&mut self) -> impl Iterator<Item = T> + use<T> {
        self.stats.invalidated +=
            u64::from(self.first.is_some()) + u64::from(self.second.is_some());
        self.first
            .take()
            .into_iter()
            .chain(self.second.take())
            .map(|(_, item)| item)
    }
    pub fn take(&mut self, now: Instant) -> (Option<T>, Duration, usize) {
        let (item, age, discarded) = self.pop(now);
        (item, age, usize::from(discarded.is_some()))
    }
    /// Select immediately, returning queue age and any stale predecessor.
    /// An isolated or final frame is always eligible, even after a long pause.
    pub fn pop(&mut self, now: Instant) -> (Option<T>, Duration, Option<T>) {
        let dropped = if self.second.is_some()
            && self.first.as_ref().is_some_and(|(time, _)| {
                self.maximum_age
                    .is_some_and(|limit| now.saturating_duration_since(*time) > limit)
            }) {
            self.stats.stale += 1;
            let dropped = self.first.take().map(|(_, item)| item);
            self.first = self.second.take();
            dropped
        } else {
            None
        };
        let selected = self.first.take();
        self.stats.selected += u64::from(selected.is_some());
        self.first = self.second.take();
        match selected {
            Some((time, item)) => (Some(item), now.saturating_duration_since(time), dropped),
            None => (None, Duration::ZERO, dropped),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicted_and_drained_owners_can_be_released_after_unlocking() {
        use std::sync::{
            Arc, Mutex, Weak,
            atomic::{AtomicUsize, Ordering},
        };
        struct Owner {
            queue: Weak<Mutex<PresentationMailbox<Owner>>>,
            released: Arc<AtomicUsize>,
        }
        impl Drop for Owner {
            fn drop(&mut self) {
                let queue = self.queue.upgrade().expect("queue alive");
                assert!(
                    queue.try_lock().is_ok(),
                    "native release must run outside queue lock"
                );
                self.released.fetch_add(1, Ordering::Relaxed);
            }
        }
        let queue = Arc::new(Mutex::new(PresentationMailbox::default()));
        let released = Arc::new(AtomicUsize::new(0));
        let owner = || Owner {
            queue: Arc::downgrade(&queue),
            released: released.clone(),
        };
        let now = Instant::now();
        queue.lock().expect("lock").push(owner(), now);
        let superseded = queue.lock().expect("lock").push(owner(), now);
        drop(superseded);
        let retired = queue.lock().expect("lock").drain();
        drop(retired);
        assert_eq!(released.load(Ordering::Relaxed), 2);
        assert_eq!(
            queue.lock().expect("lock").stats(),
            PresentationQueueStats {
                submitted: 2,
                superseded: 1,
                invalidated: 1,
                ..Default::default()
            }
        );
    }

    #[test]
    fn queue_outcomes_account_for_every_submitted_snapshot() {
        let now = Instant::now();
        let mut queue = PresentationMailbox::smoothing(Duration::from_millis(10));
        for item in 1..=3 {
            queue.push(item, now);
        }
        assert_eq!(
            queue.pop(now + Duration::from_millis(30)),
            (Some(3), Duration::from_millis(30), Some(2))
        );
        queue.push(4, now);
        queue.push(5, now);
        queue.keep_latest();
        queue.clear();
        let stats = queue.stats();
        assert_eq!(
            stats,
            PresentationQueueStats {
                submitted: 5,
                selected: 1,
                superseded: 1,
                stale: 1,
                invalidated: 2
            }
        );
    }
    #[test]
    fn an_isolated_or_final_frame_never_waits_for_prefill_or_expires() {
        let now = Instant::now();
        let mut queue = PresentationMailbox::smoothing(Duration::from_nanos(1_000_000_000 / 90));
        queue.push(1, now);
        assert_eq!(queue.take(now).0, Some(1));
        queue.push(2, now);
        assert_eq!(queue.take(now + Duration::from_secs(1)).0, Some(2));
        assert_eq!(queue.take(now + Duration::from_secs(1)).0, None);
    }
    #[test]
    fn age_budget_follows_refresh_but_slow_displays_do_not_replay_long_history() {
        let now = Instant::now();
        let mut queue = PresentationMailbox::smoothing(Duration::from_secs(1));
        queue.push(1, now);
        queue.push(2, now + Duration::from_millis(39));
        assert_eq!(queue.take(now + Duration::from_millis(40)).0, Some(2));
        queue.set_interval(Duration::from_nanos(1_000_000_000 / 120));
        queue.push(3, now);
        queue.push(4, now + Duration::from_millis(17));
        assert_eq!(queue.take(now + Duration::from_millis(18)).0, Some(4));
    }
    #[test]
    fn jitter_slot_absorbs_a_pair_without_building_a_backlog() {
        let now = Instant::now();
        let mut queue = PresentationMailbox::smoothing(Duration::from_nanos(1_000_000_000 / 90));
        assert_eq!(queue.push(1, now), None);
        assert_eq!(queue.push(2, now), None);
        assert_eq!(queue.take(now).0, Some(1));
        assert_eq!(queue.take(now + Duration::from_millis(11)).0, Some(2));
        for item in [3, 4, 5] {
            queue.push(item, now);
        }
        assert_eq!(queue.take(now).0, Some(4));
        assert_eq!(queue.take(now).0, Some(5));
    }
    #[test]
    fn latest_mode_clear_and_stalled_resume_never_replay_old_frames() {
        let now = Instant::now();
        let mut queue = PresentationMailbox::default();
        queue.push(1, now);
        assert_eq!(queue.push(2, now), Some(1));
        assert_eq!(queue.take(now).0, Some(2));
        let mut queue = PresentationMailbox::smoothing(Duration::from_nanos(1_000_000_000 / 90));
        queue.push(1, now);
        queue.push(2, now + Duration::from_millis(20));
        let (frame, age, dropped) = queue.take(now + Duration::from_millis(30));
        assert_eq!(
            (frame, age, dropped),
            (Some(2), Duration::from_millis(10), 1)
        );
        queue.push(3, now);
        queue.reset(4);
        assert_eq!(queue.take(Instant::now()).0, Some(4));
        assert_eq!(queue.take(Instant::now()).0, None);
    }
}
