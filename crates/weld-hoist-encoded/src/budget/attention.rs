//! Quality priority outlives queue priority so pauses do not repeatedly recreate
//! codecs. Focus-only handoff settles atomically; actual interaction bypasses it.

use crate::activity::{Group, GroupAttention, Priority};
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

/// Local bitrate preferences, independent from frame-admission scheduling.
#[derive(Clone, Copy, Debug)]
pub struct BitrateAllocationPolicy {
    /// Background, focused, initial-motion, interactive group weights.
    pub weights: [u16; 4],
    /// Retain quality through gaps in discrete input and sustained motion.
    pub interaction_hold: Duration,
    /// Transfer a focus-only bonus once the new group remains focused this long.
    pub focus_settle: Duration,
}

impl Default for BitrateAllocationPolicy {
    fn default() -> Self {
        Self {
            weights: [1, 2, 2, 12],
            interaction_hold: Duration::from_secs(10),
            focus_settle: Duration::from_millis(500),
        }
    }
}

impl BitrateAllocationPolicy {
    pub(super) fn validate(self) -> Result<()> {
        ensure!(
            self.weights.iter().all(|weight| *weight > 0),
            "bitrate priority weights must be positive"
        );
        for hold in [self.interaction_hold, self.focus_settle] {
            ensure!(
                !hold.is_zero() && hold <= Duration::from_secs(60),
                "bitrate priority holds must be positive and at most 60 seconds"
            );
        }
        Ok(())
    }

    pub(super) fn priority(
        self,
        mut attention: GroupAttention,
        settled_focus: bool,
        now: Instant,
    ) -> Priority {
        attention.focused = settled_focus;
        attention.sustained_motion = attention.quality_motion;
        let priority = attention.priority(now, self.interaction_hold, self.interaction_hold);
        // Initial motion must not create an intermediate rate while focus is
        // settling. Sustained motion and discrete input are immediately eligible.
        if priority == Priority::Moving && !settled_focus {
            Priority::Background
        } else {
            priority
        }
    }
}

#[derive(Default)]
pub(super) struct QualityFocus {
    pub settled: Option<Group>,
    candidate: Option<(Group, Instant)>,
}

impl QualityFocus {
    pub fn observe(
        &mut self,
        attention: &HashMap<Group, GroupAttention>,
        now: Instant,
        policy: BitrateAllocationPolicy,
    ) {
        if self
            .settled
            .is_some_and(|group| !attention.contains_key(&group))
        {
            self.settled = None;
        }
        // Include mapped groups awaiting their first stream: treating one as a
        // clear would temporarily redistribute the old bonus before registration.
        let focused = attention
            .iter()
            .find(|(_, value)| value.focused)
            .map(|(group, value)| (*group, *value));
        let Some((group, value)) = focused else {
            self.settled = None;
            self.candidate = None;
            return;
        };
        if self.settled == Some(group) {
            self.candidate = None;
        } else if policy.priority(value, false, now) == Priority::Interactive {
            self.settled = Some(group);
            self.candidate = None;
        } else if self
            .candidate
            .is_none_or(|(candidate, _)| candidate != group)
        {
            self.candidate = Some((group, now));
        }
        self.advance(now, policy.focus_settle);
    }

    pub fn advance(&mut self, now: Instant, settle: Duration) {
        if let Some((group, since)) = self.candidate
            && now.saturating_duration_since(since) >= settle
        {
            self.settled = Some(group);
            self.candidate = None;
        }
    }
}
