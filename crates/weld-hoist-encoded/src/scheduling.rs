//! Weighted admission between presentation groups. Running work is never preempted.

use crate::activity::{Activity, ActivitySnapshot, Group, Priority, SchedulingPolicy};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use weld_client::ClientSurfaceId;

#[derive(Default)]
struct Service {
    finish: u64,
    waiting: Option<Instant>,
    priority: Option<Priority>,
}

#[derive(Clone, Copy)]
pub(crate) struct Selection {
    pub surface: ClientSurfaceId,
    group: Group,
    finish: u64,
    priority: Priority,
    aged: bool,
}

#[derive(Default)]
pub(crate) struct Scheduler {
    snapshot: ActivitySnapshot,
    groups: HashMap<Group, Service>,
    selections: [u64; 4],
    aging: u64,
    last_report: Option<Instant>,
    oldest_candidate_age: Duration,
}

impl Scheduler {
    /// Candidates are one FIFO-ready member per surface, in stable rotation order.
    /// Busy exclusions affect this attempt only; selection never spends service.
    pub(crate) fn select(
        &mut self,
        candidates: &[ClientSurfaceId],
        excluded: &[ClientSurfaceId],
        activity: &Activity,
        policy: SchedulingPolicy,
        now: Instant,
    ) -> Option<Selection> {
        activity.snapshot(policy, &mut self.snapshot);
        let snapshot = &self.snapshot;
        self.groups.retain(|group, _| {
            candidates
                .iter()
                .any(|surface| snapshot.groups.get(surface) == Some(group))
        });
        let floor = self
            .groups
            .values()
            .map(|service| service.finish)
            .min()
            .unwrap_or(0);
        for surface in candidates {
            if let Some(group) = snapshot.groups.get(surface).copied() {
                let priority = snapshot
                    .attention
                    .get(&group)
                    .copied()
                    .unwrap_or_default()
                    .priority(now, policy.interaction_grace, policy.motion_grace);
                let entry = self.groups.entry(group).or_insert_with(|| Service {
                    finish: floor,
                    ..Default::default()
                });
                // Temporary exclusions retain service debt, but unavailable
                // groups (including paused presentations) must not accrue age.
                if candidates.iter().any(|candidate| {
                    !excluded.contains(candidate) && snapshot.groups.get(candidate) == Some(&group)
                }) {
                    entry.waiting.get_or_insert(now);
                } else {
                    entry.waiting = None;
                }
                if entry.priority.is_none_or(|previous| priority > previous) {
                    entry.finish = floor;
                }
                entry.priority = Some(priority);
            }
        }
        let mut best: Option<(Selection, Option<Instant>)> = None;
        for surface in candidates {
            if excluded.contains(surface) {
                continue;
            }
            let Some(group) = snapshot.groups.get(surface).copied() else {
                continue;
            };
            let Some(service) = self.groups.get(&group) else {
                continue;
            };
            let priority = service.priority.unwrap_or(Priority::Background);
            let aged_at = service
                .waiting
                .filter(|since| now.saturating_duration_since(*since) >= policy.starvation_age);
            let cost = 65536 / u64::from(policy.weights[priority.index()].max(1));
            let selection = Selection {
                surface: *surface,
                group,
                finish: service.finish.saturating_add(cost),
                priority,
                aged: aged_at.is_some(),
            };
            let better = best.as_ref().is_none_or(|(previous, previous_age)| {
                match (aged_at, *previous_age) {
                    (Some(at), Some(before)) => at < before,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => selection.finish < previous.finish,
                }
            });
            if better {
                best = Some((selection, aged_at));
            }
        }
        self.oldest_candidate_age = self
            .groups
            .values()
            .filter_map(|service| service.waiting)
            .map(|since| now.saturating_duration_since(since))
            .max()
            .unwrap_or_default();
        best.map(|(selection, _)| selection)
    }

    pub(crate) fn accepted(&mut self, selection: Selection, now: Instant) {
        if let Some(service) = self.groups.get_mut(&selection.group) {
            service.finish = selection.finish;
            service.waiting = Some(now);
        }
        let floor = self
            .groups
            .values()
            .map(|service| service.finish)
            .min()
            .unwrap_or(0);
        for service in self.groups.values_mut() {
            service.finish -= floor;
        }
        self.selections[selection.priority.index()] =
            self.selections[selection.priority.index()].saturating_add(1);
        self.aging = self.aging.saturating_add(u64::from(selection.aged));
    }

    pub(crate) fn forget(&mut self, activity: &Activity, surface: ClientSurfaceId) {
        if let Some(group) = activity.group(surface)
            && group.root == surface
        {
            self.groups.remove(&group);
        }
    }

    pub(crate) fn report(&mut self, side: &'static str, now: Instant) {
        if self
            .last_report
            .is_some_and(|last| now.saturating_duration_since(last).as_secs() < 1)
            || self.selections == [0; 4]
        {
            return;
        }
        tracing::debug!(target: "weld_media_diag", side, background = self.selections[0], focused = self.selections[1], moving = self.selections[2], interactive = self.selections[3], aging_selections = self.aging, groups_at_last_scan = self.groups.len(), oldest_candidate_age_at_last_scan_us = self.oldest_candidate_age.as_micros(), "encoded scheduling observations");
        self.selections = [0; 4];
        self.aging = 0;
        self.last_report = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::tests::{focus, mapped, press, surface};

    #[test]
    fn excluded_groups_keep_service_debt_without_accruing_starvation_age() {
        let now = Instant::now();
        let mut activity = mapped();
        focus(&mut activity, 1, now);
        press(&mut activity, 1, now);
        let policy = SchedulingPolicy::default();
        let mut scheduler = Scheduler::default();
        let candidates = [surface(2), surface(1)];
        let selected = scheduler
            .select(&candidates, &[], &activity, policy, now)
            .expect("selected");
        scheduler.accepted(selected, now);
        let group = activity.group(surface(1)).expect("group");
        let debt = scheduler.groups[&group].finish;
        let later = now + Duration::from_millis(50);
        scheduler
            .select(&candidates, &[surface(1)], &activity, policy, later)
            .expect("other group");
        assert_eq!(scheduler.groups[&group].finish, debt);
        assert!(scheduler.groups[&group].waiting.is_none());
        scheduler
            .select(&candidates, &[], &activity, policy, later)
            .expect("resume");
        assert_eq!(scheduler.groups[&group].finish, debt);
        assert_eq!(scheduler.groups[&group].waiting, Some(later));
    }

    #[test]
    fn priority_decay_keeps_service_debt_even_across_repeated_scans() {
        let now = Instant::now();
        let mut activity = mapped();
        focus(&mut activity, 1, now);
        press(&mut activity, 1, now);
        let policy = SchedulingPolicy {
            starvation_age: Duration::MAX,
            ..Default::default()
        };
        let mut scheduler = Scheduler::default();
        let candidates = [surface(2), surface(1)];
        for _ in 0..4 {
            let selected = scheduler
                .select(&candidates, &[], &activity, policy, now)
                .expect("ready");
            assert_eq!(selected.surface, surface(1));
            scheduler.accepted(selected, now);
        }
        let foreground = activity.group(surface(1)).expect("group");
        let debt = scheduler.groups[&foreground].finish;
        assert!(debt > 0);
        for _ in 0..3 {
            let selected = scheduler
                .select(
                    &candidates,
                    &[],
                    &activity,
                    policy,
                    now + Duration::from_secs(1),
                )
                .expect("ready");
            assert_eq!(selected.surface, surface(2));
            assert_eq!(scheduler.groups[&foreground].finish, debt);
            assert_eq!(
                scheduler.groups[&foreground].priority,
                Some(Priority::Focused)
            );
        }
    }

    #[test]
    fn interactive_share_is_weighted_and_busy_does_not_spend_it() {
        let now = Instant::now();
        let mut activity = mapped();
        focus(&mut activity, 1, now);
        press(&mut activity, 1, now);
        let mut scheduler = Scheduler::default();
        let policy = SchedulingPolicy::default();
        let candidates = [surface(2), surface(1)];
        let first = scheduler
            .select(&candidates, &[], &activity, policy, now)
            .expect("ready");
        assert_eq!(first.surface, surface(1));
        let retry = scheduler
            .select(&candidates, &[], &activity, policy, now)
            .expect("ready");
        assert_eq!(retry.finish, first.finish);
        let background = scheduler
            .select(&candidates, &[surface(1)], &activity, policy, now)
            .expect("other worker");
        assert_eq!(background.surface, surface(2));
        let mut selected = [0; 2];
        for _ in 0..90 {
            let chosen = scheduler
                .select(&candidates, &[], &activity, policy, now)
                .expect("ready");
            selected[usize::from(chosen.surface == surface(2))] += 1;
            scheduler.accepted(chosen, now);
        }
        assert_eq!(selected, [80, 10]);
    }

    #[test]
    fn aging_overrides_weight_without_popup_entitlement_multiplication() {
        let now = Instant::now();
        let mut activity = mapped();
        focus(&mut activity, 1, now);
        press(&mut activity, 1, now);
        activity.role(
            surface(3),
            weld_client::ClientSurfaceRole::Popup(weld_client::PopupState {
                owner: surface(1),
                position: weld_client::LogicalPoint::new(0.0, 0.0),
                stack_index: 0,
            }),
        );
        let mut scheduler = Scheduler::default();
        let policy = SchedulingPolicy::default();
        let candidates = [surface(1), surface(2), surface(3)];
        let first = scheduler
            .select(&candidates, &[], &activity, policy, now)
            .expect("ready");
        scheduler.accepted(first, now + std::time::Duration::from_millis(1));
        let chosen = scheduler
            .select(
                &candidates,
                &[],
                &activity,
                policy,
                now + policy.starvation_age,
            )
            .expect("aged");
        assert_eq!(chosen.surface, surface(2));
        assert!(chosen.aged);
        assert_eq!(scheduler.groups.len(), 2);
    }
}
