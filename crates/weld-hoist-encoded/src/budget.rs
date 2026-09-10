//! Host-owned allocation of encoder targets, not wire-rate policing. Ports share
//! numeric inventory only; weak actuators never retain codecs or client buffers.

mod allocation;
mod attention;

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    rc::{Rc, Weak},
    time::Instant,
};

use anyhow::{Context, Result, ensure};
use weld_media::MediaStreamId;

use crate::{
    EncoderBitrateLimits, EncoderRateControl,
    activity::{Group, GroupAttention, Priority},
};
use allocation::{AllocationInput, allocate};
pub use attention::BitrateAllocationPolicy;
use attention::QualityFocus;

type StreamKey = (u64, MediaStreamId);

/// The numeric minimum reservations cannot fit. No implicit overcommit is allowed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InsufficientBitrateBudget {
    pub target: u64,
    pub required: u128,
}

impl fmt::Display for InsufficientBitrateBudget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "encoder minimum targets require {} bit/s but shared target is {} bit/s",
            self.required, self.target
        )
    }
}

impl std::error::Error for InsufficientBitrateBudget {}

/// Desired targets only. Old generations and queued bytes can exceed these values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BitrateBudgetSnapshot {
    pub target: u64,
    pub allocated: u64,
    pub streams: usize,
}

/// Clone this one handle into all encoded source ports sharing a runtime budget.
///
/// Intentionally host-thread-only. The independent [`EncoderRateControl`] remains
/// Send + Sync, but manual requests are rejected after a budget takes ownership.
/// Static streams retain a share; this does not estimate network or GPU capacity.
#[derive(Clone)]
pub struct SharedBitrateBudget(Rc<Coordinator>);

struct Coordinator {
    state: RefCell<BudgetState>,
    // Outside the RefCell so Drop can always invalidate inventory during a borrow.
    dirty: Cell<bool>,
}

struct BudgetState {
    target: u64,
    policy: BitrateAllocationPolicy,
    next_port: Option<u64>,
    ports: BTreeMap<u64, PortInventory>,
    targets: BTreeMap<StreamKey, u64>,
    priorities: BTreeMap<(u64, Group), Priority>,
}

struct PortInventory {
    live: Weak<()>,
    control: EncoderRateControl,
    limits: EncoderBitrateLimits,
    demands: Vec<StreamDemand>,
    attention: BTreeMap<Group, GroupAttention>,
    focus: QualityFocus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StreamDemand {
    pub stream: MediaStreamId,
    pub group: Group,
    // Full input buffer pixels, not logical/cropped geometry or codec padding.
    pub pixels: u64,
}

pub(crate) struct BudgetMembership {
    budget: SharedBitrateBudget,
    id: u64,
    live: Option<Rc<()>>,
}

impl SharedBitrateBudget {
    pub fn new(bits_per_second: u64) -> Result<Self> {
        Self::with_policy(bits_per_second, BitrateAllocationPolicy::default())
    }

    /// Validate and inject quality policy for all ports sharing this pool.
    pub fn with_policy(bits_per_second: u64, policy: BitrateAllocationPolicy) -> Result<Self> {
        policy.validate()?;
        ensure!(
            bits_per_second > 0,
            "shared bitrate target must be positive"
        );
        Ok(Self(Rc::new(Coordinator {
            state: RefCell::new(BudgetState {
                target: bits_per_second,
                policy,
                next_port: Some(1),
                ports: BTreeMap::new(),
                targets: BTreeMap::new(),
                priorities: BTreeMap::new(),
            }),
            dirty: Cell::new(false),
        })))
    }

    /// Set a runtime target. Existing jobs remain frozen and switches apply lazily.
    /// An impossible target leaves the previous configuration intact.
    pub fn set_target(&self, bits_per_second: u64) -> Result<()> {
        let now = Instant::now();
        ensure!(
            bits_per_second > 0,
            "shared bitrate target must be positive"
        );
        self.refresh_at(now)?;
        {
            let mut state = self.0.state.try_borrow_mut()?;
            state.check_minimum(bits_per_second, None)?;
            if state.target != bits_per_second {
                state.target = bits_per_second;
                self.0.dirty.set(true);
            }
        }
        self.refresh_at(now)
    }

    pub fn snapshot(&self) -> Result<BitrateBudgetSnapshot> {
        self.refresh_at(Instant::now())?;
        let state = self.0.state.try_borrow()?;
        Ok(BitrateBudgetSnapshot {
            target: state.target,
            allocated: state.targets.values().sum(),
            streams: state.targets.len(),
        })
    }

    pub(crate) fn attach(&self, control: EncoderRateControl) -> Result<BudgetMembership> {
        self.refresh_at(Instant::now())?;
        let limits = control.limits()?;
        let live = Rc::new(());
        let mut state = self.0.state.try_borrow_mut()?;
        let id = state
            .next_port
            .context("bitrate budget port identity exhausted")?;
        control.manage()?;
        state.next_port = id.checked_add(1);
        state.ports.insert(
            id,
            PortInventory {
                live: Rc::downgrade(&live),
                control,
                limits,
                demands: Vec::new(),
                attention: BTreeMap::new(),
                focus: QualityFocus::default(),
            },
        );
        Ok(BudgetMembership {
            budget: self.clone(),
            id,
            live: Some(live),
        })
    }

    fn refresh_at(&self, now: Instant) -> Result<()> {
        let original_targets = {
            let mut state = self.0.state.try_borrow_mut()?;
            let policy = state.policy;
            for port in state.ports.values_mut() {
                port.focus.advance(now, policy.focus_settle);
            }
            // Ordinary host ticks only compare existing group entries. Refreshed
            // input timestamps extend leases without allocating or rebalancing.
            let changed = state.ports.iter().any(|(id, port)| {
                port.attention.iter().any(|(group, attention)| {
                    state.priorities.get(&(*id, *group))
                        != Some(&policy.priority(
                            *attention,
                            port.focus.settled == Some(*group),
                            now,
                        ))
                })
            });
            if !self.0.dirty.get() && !changed {
                return Ok(());
            }
            self.0.dirty.set(true);
            state.targets.clone()
        };
        loop {
            let (publications, targets, priorities) = {
                let mut state = self.0.state.try_borrow_mut()?;
                state.ports.retain(|_, port| port.live.strong_count() > 0);
                let policy = state.policy;
                let priorities = state
                    .ports
                    .iter()
                    .flat_map(|(id, port)| {
                        port.attention.iter().map(move |(group, attention)| {
                            (
                                (*id, *group),
                                policy.priority(
                                    *attention,
                                    port.focus.settled == Some(*group),
                                    now,
                                ),
                            )
                        })
                    })
                    .collect::<BTreeMap<_, _>>();
                let inputs = state
                    .ports
                    .iter()
                    .flat_map(|(id, port)| {
                        port.demands.iter().map(|demand| AllocationInput {
                            key: (*id, demand.stream),
                            group: (*id, demand.group),
                            pixels: demand.pixels,
                            limits: port.limits,
                            current: original_targets.get(&(*id, demand.stream)).copied(),
                            weight: u64::from(
                                policy.weights[priorities
                                    .get(&(*id, demand.group))
                                    .copied()
                                    .unwrap_or(Priority::Background)
                                    .index()],
                            ),
                        })
                    })
                    .collect::<Vec<_>>();
                let targets = allocate(state.target, &inputs)?;
                let publications = inputs
                    .iter()
                    .zip(&targets)
                    .map(|(input, rate)| {
                        let port = state
                            .ports
                            .get(&input.key.0)
                            .context("bitrate budget port disappeared")?;
                        Ok((input.key.0, port.control.clone(), input.key.1, *rate))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let targets = inputs
                    .iter()
                    .zip(targets)
                    .map(|(input, rate)| (input.key, rate))
                    .collect();
                (publications, targets, priorities)
            };
            // No coordinator borrow or native work spans these numeric registry writes.
            // A failed actuator belongs to one source, not every peer in the pool.
            // Intermediate surviving requests can be overwritten on the next pass;
            // host-owned frame preparation cannot interleave with this loop.
            let mut failed = BTreeSet::new();
            for (port, control, stream, rate) in publications {
                if failed.contains(&port) {
                    continue;
                }
                if let Err(error) = control.request_managed(stream, rate) {
                    failed.insert(port);
                    tracing::warn!(port, %error, "removing failed encoder actuator from shared bitrate budget");
                }
            }
            if failed.is_empty() {
                let mut state = self.0.state.try_borrow_mut()?;
                state.targets = targets;
                state.priorities = priorities;
                self.0.dirty.set(false);
                return Ok(());
            }
            let mut state = self.0.state.try_borrow_mut()?;
            for port in failed {
                state.ports.remove(&port);
                state.targets.retain(|(id, _), _| *id != port);
                state.priorities.retain(|(id, _), _| *id != port);
            }
            // Every unsuccessful pass removes at least one member. Allocation errors
            // propagate above without removing members or silently overcommitting.
        }
    }
}

impl BudgetState {
    fn check_minimum(&self, target: u64, replacement_count: Option<(u64, usize)>) -> Result<()> {
        let required = self
            .ports
            .iter()
            .filter(|(_, port)| port.live.strong_count() > 0)
            .try_fold(0_u128, |sum, (id, port)| {
                let count = replacement_count
                    .filter(|(candidate, _)| candidate == id)
                    .map_or(port.demands.len(), |(_, count)| count);
                sum.checked_add(u128::from(port.limits.minimum()) * count as u128)
                    .context("bitrate reservation sum overflow")
            })?;
        if required > u128::from(target) {
            return Err(InsufficientBitrateBudget { target, required }.into());
        }
        Ok(())
    }
}

impl BudgetMembership {
    /// Preflight before source IDs/registry entries are allocated.
    pub(crate) fn preflight(&self, count: usize, now: Instant) -> Result<()> {
        self.budget.refresh_at(now)?;
        let state = self.budget.0.state.try_borrow()?;
        ensure!(
            state.ports.contains_key(&self.id),
            "bitrate budget membership retired"
        );
        state.check_minimum(state.target, Some((self.id, count)))
    }

    /// Reconcile only structural changes, then publish attention before refreshing.
    pub(crate) fn update_inventory(&self, demands: Vec<StreamDemand>) -> Result<()> {
        {
            let mut state = self.budget.0.state.try_borrow_mut()?;
            state.check_minimum(state.target, Some((self.id, demands.len())))?;
            let port = state
                .ports
                .get_mut(&self.id)
                .context("bitrate budget membership retired")?;
            if port.demands != demands {
                port.attention
                    .retain(|group, _| demands.iter().any(|demand| demand.group == *group));
                for demand in &demands {
                    port.attention.entry(demand.group).or_default();
                }
                port.demands = demands;
                self.budget.0.dirty.set(true);
            }
        }
        Ok(())
    }

    pub(crate) fn update_attention(
        &self,
        attention: &HashMap<Group, GroupAttention>,
        now: Instant,
    ) -> Result<()> {
        {
            let mut state = self.budget.0.state.try_borrow_mut()?;
            let policy = state.policy;
            let port = state
                .ports
                .get_mut(&self.id)
                .context("bitrate budget membership retired")?;
            port.focus.observe(attention, now, policy);
            for (group, value) in &mut port.attention {
                *value = attention.get(group).copied().unwrap_or_default();
            }
        }
        self.budget.refresh_at(now)?;
        ensure!(
            self.budget
                .0
                .state
                .try_borrow()?
                .ports
                .contains_key(&self.id),
            "bitrate budget membership retired"
        );
        Ok(())
    }

    /// Last successfully allocated class; reporting must not drive allocation.
    pub(crate) fn priority(&self, group: Group) -> Option<Priority> {
        self.budget
            .0
            .state
            .try_borrow()
            .ok()?
            .priorities
            .get(&(self.id, group))
            .copied()
    }
}

impl Drop for BudgetMembership {
    fn drop(&mut self) {
        self.live.take();
        self.budget.0.dirty.set(true);
        // A borrowed coordinator is repaired on the next plain admission, update,
        // or snapshot. Never keep a dead port's demand waiting for a layout change.
        if self.budget.0.state.try_borrow_mut().is_ok()
            && let Err(error) = self.budget.refresh_at(Instant::now())
        {
            tracing::warn!(%error, "could not release shared bitrate reservation");
        }
    }
}

#[cfg(test)]
mod tests;
