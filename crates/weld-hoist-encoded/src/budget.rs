//! Host-owned allocation of encoder targets, not wire-rate policing. Ports share
//! numeric inventory only; weak actuators never retain codecs or client buffers.

mod allocation;

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    fmt,
    rc::{Rc, Weak},
};

use anyhow::{Context, Result, ensure};
use weld_media::MediaStreamId;

use crate::{EncoderBitrateLimits, EncoderRateControl, activity::Group};
use allocation::{AllocationInput, allocate};

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
    next_port: Option<u64>,
    ports: BTreeMap<u64, PortInventory>,
    targets: BTreeMap<StreamKey, u64>,
}

struct PortInventory {
    live: Weak<()>,
    control: EncoderRateControl,
    limits: EncoderBitrateLimits,
    demands: Vec<StreamDemand>,
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
        ensure!(
            bits_per_second > 0,
            "shared bitrate target must be positive"
        );
        Ok(Self(Rc::new(Coordinator {
            state: RefCell::new(BudgetState {
                target: bits_per_second,
                next_port: Some(1),
                ports: BTreeMap::new(),
                targets: BTreeMap::new(),
            }),
            dirty: Cell::new(false),
        })))
    }

    /// Set a runtime target. Existing jobs remain frozen and switches apply lazily.
    /// An impossible target leaves the previous configuration intact.
    pub fn set_target(&self, bits_per_second: u64) -> Result<()> {
        ensure!(
            bits_per_second > 0,
            "shared bitrate target must be positive"
        );
        self.refresh()?;
        {
            let mut state = self.0.state.try_borrow_mut()?;
            state.check_minimum(bits_per_second, None)?;
            if state.target != bits_per_second {
                state.target = bits_per_second;
                self.0.dirty.set(true);
            }
        }
        self.refresh()
    }

    pub fn snapshot(&self) -> Result<BitrateBudgetSnapshot> {
        self.refresh()?;
        let state = self.0.state.try_borrow()?;
        Ok(BitrateBudgetSnapshot {
            target: state.target,
            allocated: state.targets.values().sum(),
            streams: state.targets.len(),
        })
    }

    pub(crate) fn attach(&self, control: EncoderRateControl) -> Result<BudgetMembership> {
        self.refresh()?;
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
            },
        );
        Ok(BudgetMembership {
            budget: self.clone(),
            id,
            live: Some(live),
        })
    }

    fn refresh(&self) -> Result<()> {
        if !self.0.dirty.get() {
            return Ok(());
        }
        loop {
            let publications = {
                let mut state = self.0.state.try_borrow_mut()?;
                state.ports.retain(|_, port| port.live.strong_count() > 0);
                let inputs = state
                    .ports
                    .iter()
                    .flat_map(|(id, port)| {
                        port.demands.iter().map(|demand| AllocationInput {
                            key: (*id, demand.stream),
                            group: (*id, demand.group),
                            pixels: demand.pixels,
                            limits: port.limits,
                            current: state.targets.get(&(*id, demand.stream)).copied(),
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
                state.targets = inputs
                    .iter()
                    .zip(targets)
                    .map(|(input, rate)| (input.key, rate))
                    .collect();
                publications
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
                self.0.dirty.set(false);
                return Ok(());
            }
            let mut state = self.0.state.try_borrow_mut()?;
            for port in failed {
                state.ports.remove(&port);
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
    pub(crate) fn preflight(&self, count: usize) -> Result<()> {
        self.budget.refresh()?;
        let state = self.budget.0.state.try_borrow()?;
        ensure!(
            state.ports.contains_key(&self.id),
            "bitrate budget membership retired"
        );
        state.check_minimum(state.target, Some((self.id, count)))
    }

    pub(crate) fn update(&self, demands: Vec<StreamDemand>) -> Result<()> {
        {
            let mut state = self.budget.0.state.try_borrow_mut()?;
            state.check_minimum(state.target, Some((self.id, demands.len())))?;
            let port = state
                .ports
                .get_mut(&self.id)
                .context("bitrate budget membership retired")?;
            if port.demands != demands {
                port.demands = demands;
                self.budget.0.dirty.set(true);
            }
        }
        self.budget.refresh()?;
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
}

impl Drop for BudgetMembership {
    fn drop(&mut self) {
        self.live.take();
        self.budget.0.dirty.set(true);
        // A borrowed coordinator is repaired on the next plain admission, update,
        // or snapshot. Never keep a dead port's demand waiting for a layout change.
        if self.budget.0.state.try_borrow_mut().is_ok()
            && let Err(error) = self.budget.refresh()
        {
            tracing::warn!(%error, "could not release shared bitrate reservation");
        }
    }
}

#[cfg(test)]
mod tests;
