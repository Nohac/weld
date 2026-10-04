//! Per-surface callback ownership. Native presentation and adapter presentation
//! are separate consumers; neither path completes a GPU buffer-use lease.

use super::{ServerState, surface_tree::collect_surfaces};
use crate::surface::SurfaceId;
use smithay::{
    reexports::wayland_server::{
        Resource,
        protocol::{wl_callback::WlCallback, wl_surface::WlSurface},
    },
    wayland::compositor::{SurfaceAttributes, with_states},
};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
use weld_client::{
    ClientPresentationClaim, ClientPresentationUpdate, ClientSourceId, PresentationRate,
};

#[derive(Default)]
struct Demand {
    owners: HashMap<ClientSourceId, ClientPresentationClaim>,
    last: Option<Instant>,
}

#[derive(Default)]
pub(super) struct PresentationClaims {
    roots: HashMap<SurfaceId, Demand>,
    local: HashMap<SurfaceId, Option<PresentationRate>>,
    local_handoffs: HashSet<SurfaceId>,
}

impl PresentationClaims {
    fn take_local_demand(&mut self) -> bool {
        let pending = self
            .local_handoffs
            .iter()
            .any(|surface| self.native(*surface));
        self.local_handoffs.clear();
        pending
    }
    fn update(&mut self, owner: ClientSourceId, update: ClientPresentationUpdate) {
        match update.claim {
            ClientPresentationClaim::Release => {
                if let Some(demand) = self.roots.get_mut(&update.surface) {
                    demand.owners.remove(&owner);
                    if demand.owners.is_empty() {
                        self.roots.remove(&update.surface);
                    }
                }
            }
            claim => {
                self.roots
                    .entry(update.surface)
                    .or_default()
                    .owners
                    .insert(owner, claim);
            }
        }
    }

    pub(super) fn claimed(&self, surface: SurfaceId) -> bool {
        self.roots.contains_key(&surface)
    }

    /// Explicit local demand participates alongside adapter consumers. Before
    /// policy publishes it, adapter claims take over the implicit native owner.
    pub(super) fn native(&self, surface: SurfaceId) -> bool {
        let active_adapter = self.roots.get(&surface).is_some_and(|demand| {
            demand
                .owners
                .values()
                .any(|claim| matches!(claim, ClientPresentationClaim::Active { .. }))
        });
        if active_adapter {
            return false;
        }
        self.local
            .get(&surface)
            .map_or(!self.claimed(surface), Option::is_some)
    }

    fn timeout(
        &self,
        surface: SurfaceId,
        fallback: PresentationRate,
        now: Instant,
    ) -> Option<Duration> {
        if self.native(surface) {
            return None;
        }
        let demand = self.roots.get(&surface)?;
        // Each rate-less owner uses this root's output rate before aggregation.
        let rate = demand
            .owners
            .values()
            .filter_map(|claim| match claim {
                ClientPresentationClaim::Active { rate } => Some(rate.unwrap_or(fallback)),
                ClientPresentationClaim::Release | ClientPresentationClaim::Paused => None,
            })
            .chain(self.local.get(&surface).copied().flatten())
            .max()?;
        Some(demand.last.map_or(Duration::ZERO, |last| {
            (last + rate.interval()).saturating_duration_since(now)
        }))
    }

    fn completed(&mut self, surface: SurfaceId, now: Instant) {
        if let Some(demand) = self.roots.get_mut(&surface) {
            demand.last = Some(now);
        }
    }
}

pub(super) struct SurfaceCallbacks {
    pub root: SurfaceId,
    surface: WlSurface,
    callbacks: Vec<WlCallback>,
}

impl SurfaceCallbacks {
    pub(super) fn complete(self, time: u32) {
        if self.surface.is_alive() {
            for callback in self.callbacks {
                if callback.is_alive() {
                    callback.done(time);
                }
            }
        }
    }

    fn restore(mut self) {
        if self.surface.is_alive() {
            with_states(&self.surface, |states| {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                self.callbacks
                    .append(&mut attributes.current().frame_callbacks);
                attributes.current().frame_callbacks = self.callbacks;
            });
        }
    }
}

pub(super) fn take_callbacks(root: SurfaceId, surface: &WlSurface) -> Vec<SurfaceCallbacks> {
    collect_surfaces(surface)
        .into_iter()
        .filter(Resource::is_alive)
        .filter_map(|surface| {
            let callbacks = with_states(&surface, |states| {
                std::mem::take(
                    &mut states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .frame_callbacks,
                )
            });
            (!callbacks.is_empty()).then_some(SurfaceCallbacks {
                root,
                surface,
                callbacks,
            })
        })
        .collect()
}

impl ServerState {
    #[cfg(feature = "test-support")]
    pub(crate) fn staged_callback_count(&self) -> usize {
        self.staged_frame_callbacks
            .iter()
            .flat_map(|(_, groups)| groups)
            .map(|group| group.callbacks.len())
            .sum()
    }

    pub(super) fn apply_presentation_claim(
        &mut self,
        owner: ClientSourceId,
        update: ClientPresentationUpdate,
    ) {
        if self.toplevels.get(update.surface).is_none() && self.popups.get(update.surface).is_none()
        {
            return;
        }
        let was_native = self.presentation_claims.native(update.surface);
        self.presentation_claims.update(owner, update);
        self.transition_callback_owner(update.surface, was_native);
    }

    pub(super) fn set_local_presentation(
        &mut self,
        surface: SurfaceId,
        rate: Option<PresentationRate>,
    ) {
        if self.toplevels.get(surface).is_none() && self.popups.get(surface).is_none() {
            return;
        }
        let was_native = self.presentation_claims.native(surface);
        self.presentation_claims.local.insert(surface, rate);
        self.transition_callback_owner(surface, was_native);
    }

    fn transition_callback_owner(&mut self, surface: SurfaceId, was_native: bool) {
        let native = self.presentation_claims.native(surface);
        if was_native && !native {
            // Extract before a native completion can signal these callbacks.
            // Do not wait behind another output's pending ledger prefix.
            for (_, groups) in &mut self.staged_frame_callbacks {
                let mut remaining = Vec::new();
                for group in groups.drain(..) {
                    if group.root == surface {
                        self.independent_callbacks
                            .entry(surface)
                            .or_default()
                            .push(group);
                    } else {
                        remaining.push(group);
                    }
                }
                *groups = remaining;
            }
        } else if !was_native && native {
            if let Some(groups) = self.independent_callbacks.remove(&surface) {
                // Prepend oldest callbacks per actual wl_surface, not just root.
                for group in groups.into_iter().rev() {
                    group.restore();
                }
            }
            self.presentation_requested = true;
            // Current (not yet staged) callbacks need native composition too.
            let pending = self
                .mapped_frame_roots()
                .find(|(id, _)| *id == surface)
                .is_some_and(|(id, root)| self.root_has_callbacks(id, &root));
            if pending {
                self.presentation_claims.local_handoffs.insert(surface);
            }
        }
    }

    pub(crate) fn take_local_callback_demand(&mut self) -> bool {
        self.presentation_claims.take_local_demand()
    }

    fn fallback_presentation_rate(&self, surface: SurfaceId) -> PresentationRate {
        let owner = self
            .popups
            .get(surface)
            .and_then(|popup| popup.owner)
            .unwrap_or(surface);
        let preferred = self
            .toplevels
            .get(owner)
            .map_or(self.primary_output, |toplevel| toplevel.outputs.preferred);
        self.outputs
            .get(&preferred)
            .and_then(|output| output.native.current_mode())
            .and_then(|mode| u32::try_from(mode.refresh).ok())
            .and_then(|rate| PresentationRate::try_from(rate).ok())
            .unwrap_or(PresentationRate::HZ_60)
    }

    fn root_has_callbacks(&self, id: SurfaceId, root: &WlSurface) -> bool {
        self.independent_callbacks
            .get(&id)
            .is_some_and(|groups| !groups.is_empty())
            || collect_surfaces(root)
                .into_iter()
                .filter(Resource::is_alive)
                .any(|surface| {
                    with_states(&surface, |states| {
                        !states
                            .cached_state
                            .get::<SurfaceAttributes>()
                            .current()
                            .frame_callbacks
                            .is_empty()
                    })
                })
    }

    pub(crate) fn independent_callback_timeout(&self, now: Instant) -> Option<Duration> {
        self.independent_frame_roots()
            .filter(|(id, root)| self.root_has_callbacks(*id, root))
            .filter_map(|(id, _)| {
                self.presentation_claims
                    .timeout(id, self.fallback_presentation_rate(id), now)
            })
            .min()
    }

    pub(crate) fn service_independent_callbacks(&mut self, now: Instant) {
        let due = self
            .independent_frame_roots()
            .filter(|(id, root)| {
                self.root_has_callbacks(*id, root)
                    && self
                        .presentation_claims
                        .timeout(*id, self.fallback_presentation_rate(*id), now)
                        .is_some_and(|delay| delay.is_zero())
            })
            .collect::<Vec<_>>();
        let time = self.event_time();
        for (id, root) in due {
            let mut groups = self.independent_callbacks.remove(&id).unwrap_or_default();
            groups.extend(take_callbacks(id, &root));
            for group in groups {
                group.complete(time);
            }
            self.presentation_claims.completed(id, now);
        }
        // The native presentation_requested latch belongs to native staging.
    }

    fn independent_frame_roots(&self) -> impl Iterator<Item = (SurfaceId, WlSurface)> + '_ {
        self.presentation_claims.roots.keys().filter_map(|&id| {
            let (root, tree) = if let Some(toplevel) = self.toplevels.get(id) {
                if !toplevel.surface.alive() {
                    return None;
                }
                (toplevel.surface.wl_surface(), &toplevel.tree)
            } else {
                let popup = self.popups.get(id)?;
                if !popup.surface.alive() {
                    return None;
                }
                (popup.surface.wl_surface(), &popup.tree)
            };
            (root.is_alive() && tree.client_mapped(root)).then(|| (id, root.clone()))
        })
    }

    pub(super) fn forget_presentation(&mut self, surface: SurfaceId) {
        self.presentation_claims.local.remove(&surface);
        self.presentation_claims.roots.remove(&surface);
        self.presentation_claims.local_handoffs.remove(&surface);
        self.independent_callbacks.remove(&surface);
        for (_, groups) in &mut self.staged_frame_callbacks {
            groups.retain(|group| group.root != surface);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_local_demand_and_independent_consumers_aggregate_without_implicit_reactivation() {
        let mut claims = PresentationClaims::default();
        let surface = SurfaceId::for_test(8);
        let owner = ClientSourceId::new(8);
        let now = Instant::now();
        let local_rate = PresentationRate::try_from(120_000).expect("120Hz");
        claims.local.insert(surface, None);
        assert!(!claims.native(surface));
        claims.update(
            owner,
            ClientPresentationUpdate {
                surface,
                claim: ClientPresentationClaim::Active {
                    rate: Some(PresentationRate::HZ_60),
                },
            },
        );
        claims.completed(surface, now);
        assert_eq!(
            claims.timeout(surface, local_rate, now),
            Some(PresentationRate::HZ_60.interval())
        );
        claims.local.insert(surface, Some(local_rate));
        assert_eq!(
            claims.timeout(surface, local_rate, now),
            Some(local_rate.interval())
        );
        claims.update(
            owner,
            ClientPresentationUpdate {
                surface,
                claim: ClientPresentationClaim::Paused,
            },
        );
        assert!(claims.native(surface));
        assert_eq!(claims.timeout(surface, local_rate, now), None);
        claims.local.insert(surface, None);
        claims.update(
            owner,
            ClientPresentationUpdate {
                surface,
                claim: ClientPresentationClaim::Release,
            },
        );
        assert!(!claims.native(surface));
    }

    #[test]
    fn claims_are_isolated_and_only_active_consumers_supply_cadence() {
        let mut claims = PresentationClaims::default();
        let root = SurfaceId::for_test(1);
        let first = ClientSourceId::new(1);
        let second = ClientSourceId::new(2);
        let now = Instant::now();
        let rate = PresentationRate::try_from(90_000).expect("rate");
        assert_eq!(claims.timeout(root, rate, now), None);
        claims.update(
            first,
            ClientPresentationUpdate {
                surface: root,
                claim: ClientPresentationClaim::Active { rate: None },
            },
        );
        claims.completed(root, now);
        assert_eq!(claims.timeout(root, rate, now), Some(rate.interval()));
        claims.update(
            second,
            ClientPresentationUpdate {
                surface: root,
                claim: ClientPresentationClaim::Paused,
            },
        );
        claims.update(
            first,
            ClientPresentationUpdate {
                surface: root,
                claim: ClientPresentationClaim::Release,
            },
        );
        assert!(claims.claimed(root));
        assert_eq!(claims.timeout(root, rate, now), None);
        claims.update(
            second,
            ClientPresentationUpdate {
                surface: root,
                claim: ClientPresentationClaim::Release,
            },
        );
        assert!(!claims.claimed(root));
    }

    #[test]
    fn fastest_claim_includes_each_owners_output_fallback() {
        let mut claims = PresentationClaims::default();
        let surface = SurfaceId::for_test(2);
        let now = Instant::now();
        let rate = PresentationRate::try_from(360_000).expect("fast display");
        claims.update(
            ClientSourceId::new(1),
            ClientPresentationUpdate {
                surface,
                claim: ClientPresentationClaim::Active { rate: None },
            },
        );
        claims.update(
            ClientSourceId::new(2),
            ClientPresentationUpdate {
                surface,
                claim: ClientPresentationClaim::Active {
                    rate: Some(PresentationRate::HZ_60),
                },
            },
        );
        claims.completed(surface, now);
        assert_eq!(claims.timeout(surface, rate, now), Some(rate.interval()));
    }

    #[test]
    fn local_handoff_is_one_shot_and_reclaim_in_same_turn_cancels_it() {
        let mut claims = PresentationClaims::default();
        let surface = SurfaceId::for_test(3);
        claims.local_handoffs.insert(surface);
        assert!(claims.take_local_demand());
        assert!(!claims.take_local_demand());
        claims.local_handoffs.insert(surface);
        claims.update(
            ClientSourceId::new(1),
            ClientPresentationUpdate {
                surface,
                claim: ClientPresentationClaim::Active { rate: None },
            },
        );
        assert!(!claims.take_local_demand());
        claims.update(
            ClientSourceId::new(1),
            ClientPresentationUpdate {
                surface,
                claim: ClientPresentationClaim::Release,
            },
        );
        assert!(
            !claims.take_local_demand(),
            "cancelled demand cannot survive a later release"
        );
    }

    #[test]
    fn rate_changes_use_last_completion_and_late_ticks_do_not_catch_up() {
        let mut claims = PresentationClaims::default();
        let surface = SurfaceId::for_test(1);
        let owner = ClientSourceId::new(1);
        let now = Instant::now();
        let rate = PresentationRate::try_from(90_000).expect("rate");
        claims.update(
            owner,
            ClientPresentationUpdate {
                surface,
                claim: ClientPresentationClaim::Active {
                    rate: Some(PresentationRate::HZ_60),
                },
            },
        );
        claims.completed(surface, now);
        claims.update(
            owner,
            ClientPresentationUpdate {
                surface,
                claim: ClientPresentationClaim::Active { rate: Some(rate) },
            },
        );
        assert_eq!(
            claims.timeout(surface, PresentationRate::HZ_60, now),
            Some(rate.interval())
        );
        let later = now + Duration::from_secs(1);
        assert_eq!(claims.timeout(surface, rate, later), Some(Duration::ZERO));
        claims.completed(surface, later);
        assert_eq!(claims.timeout(surface, rate, later), Some(rate.interval()));
    }
}
