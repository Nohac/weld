//! One extra presentation slot absorbs arrival jitter without unbounded replay.
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

pub(super) struct Mailbox<T> {
    items: VecDeque<(Instant, T)>,
    maximum_age: Option<Duration>,
}
impl<T> Default for Mailbox<T> {
    fn default() -> Self {
        Self {
            items: VecDeque::new(),
            maximum_age: None,
        }
    }
}
impl<T> Mailbox<T> {
    pub fn smoothing(interval: Duration) -> Self {
        Self {
            maximum_age: Some(interval.saturating_mul(2).min(Duration::from_millis(34))),
            ..Self::default()
        }
    }
    pub fn push(&mut self, item: T, now: Instant) -> Option<T> {
        let removed = if self.items.len() >= if self.maximum_age.is_some() { 2 } else { 1 } {
            self.items.pop_front().map(|(_, value)| value)
        } else {
            None
        };
        self.items.push_back((now, item));
        removed
    }
    pub fn newest_mut(&mut self) -> Option<&mut T> {
        self.items.back_mut().map(|(_, value)| value)
    }
    pub fn set_interval(&mut self, interval: Duration) {
        if self.maximum_age.is_some() {
            self.maximum_age = Some(interval.saturating_mul(2).min(Duration::from_millis(34)));
        }
    }
    pub fn reset(&mut self, item: T) {
        self.clear();
        self.push(item, Instant::now());
    }
    pub fn clear(&mut self) {
        self.items.clear();
    }
    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.items.drain(..).map(|(_, item)| item)
    }
    pub fn take(&mut self, now: Instant) -> (Option<T>, Duration, usize) {
        let (item, age, discarded) = self.pop(now);
        (item, age, usize::from(discarded.is_some()))
    }
    pub fn pop(&mut self, now: Instant) -> (Option<T>, Duration, Option<T>) {
        let mut dropped = None;
        // A stalled renderer resumes at the newest frame, never replays history.
        if self.items.len() == 2
            && self.items.front().is_some_and(|(time, _)| {
                self.maximum_age
                    .is_some_and(|limit| now.saturating_duration_since(*time) > limit)
            })
        {
            dropped = self.items.pop_front().map(|(_, item)| item);
        }
        match self.items.pop_front() {
            Some((time, item)) => (Some(item), now.saturating_duration_since(time), dropped),
            None => (None, Duration::ZERO, dropped),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_isolated_or_final_frame_never_waits_for_prefill_or_expires() {
        let now = Instant::now();
        let mut queue = Mailbox::smoothing(Duration::from_nanos(1_000_000_000 / 90));
        queue.push(1, now);
        assert_eq!(queue.take(now).0, Some(1));
        queue.push(2, now);
        assert_eq!(queue.take(now + Duration::from_secs(1)).0, Some(2));
        assert_eq!(queue.take(now + Duration::from_secs(1)).0, None);
    }
    #[test]
    fn age_budget_follows_refresh_but_slow_displays_do_not_replay_long_history() {
        let now = Instant::now();
        let mut queue = Mailbox::smoothing(Duration::from_secs(1));
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
        let mut queue = Mailbox::smoothing(Duration::from_nanos(1_000_000_000 / 90));
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
        let mut queue = Mailbox::default();
        queue.push(1, now);
        assert_eq!(queue.push(2, now), Some(1));
        assert_eq!(queue.take(now).0, Some(2));
        let mut queue = Mailbox::smoothing(Duration::from_nanos(1_000_000_000 / 90));
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
