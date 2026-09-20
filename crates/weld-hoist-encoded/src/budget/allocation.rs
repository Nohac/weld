//! Two-level capped allocation with asymmetric churn suppression. Recomputed on
//! inventory/target or effective priority changes, not every input event.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};

use super::{AllocationGroup, EncoderBitrateLimits, InsufficientBitrateBudget, StreamKey};

const RATE_STEP: u64 = 64_000;

pub(super) struct AllocationInput {
    pub key: StreamKey,
    pub group: (u64, AllocationGroup),
    pub pixels: u64,
    pub area_weight: u16,
    pub limits: EncoderBitrateLimits,
    pub current: Option<u64>,
    pub weight: u64,
}

struct Share {
    minimum: u64,
    maximum: u64,
    weight: u64,
}

/// Reserve minima, repeatedly redistribute clipped shares, then distribute the
/// integer remainder in stable order. Work is bounded by entries, not bit rate.
fn waterfill(target: u64, shares: &[Share]) -> Result<Vec<u64>> {
    let minimum = shares
        .iter()
        .map(|share| u128::from(share.minimum))
        .sum::<u128>();
    if minimum > u128::from(target) {
        return Err(InsufficientBitrateBudget {
            target,
            required: minimum,
        }
        .into());
    }
    let mut rates = shares.iter().map(|share| share.minimum).collect::<Vec<_>>();
    let mut remaining = target - u64::try_from(minimum)?;
    while remaining > 0 {
        let weight = shares
            .iter()
            .zip(&rates)
            .filter(|(share, rate)| **rate < share.maximum)
            .map(|(share, _)| u128::from(share.weight))
            .sum::<u128>();
        if weight == 0 {
            break;
        }
        let available = remaining;
        for (share, rate) in shares.iter().zip(&mut rates) {
            if *rate == share.maximum {
                continue;
            }
            let extra = u64::try_from(u128::from(available) * u128::from(share.weight) / weight)?
                .min(share.maximum - *rate);
            *rate += extra;
            remaining -= extra;
        }
        if remaining == available {
            for (share, rate) in shares.iter().zip(&mut rates) {
                if remaining == 0 {
                    break;
                }
                if *rate < share.maximum {
                    *rate += 1;
                    remaining -= 1;
                }
            }
        }
    }
    Ok(rates)
}

pub(super) fn allocate(target: u64, inputs: &[AllocationInput]) -> Result<Vec<u64>> {
    let mut groups = BTreeMap::new();
    for (index, input) in inputs.iter().enumerate() {
        ensure!(input.pixels > 0, "bitrate demand has zero buffer pixels");
        groups
            .entry(input.group)
            .or_insert_with(Vec::new)
            .push(index);
    }
    let shares = groups
        .values()
        .map(|members| {
            let minimum = members
                .iter()
                .map(|index| u128::from(inputs[*index].limits.minimum()))
                .sum::<u128>();
            let maximum = members
                .iter()
                .map(|index| u128::from(inputs[*index].limits.maximum()))
                .sum::<u128>();
            if minimum > u128::from(target) {
                return Err(InsufficientBitrateBudget {
                    target,
                    required: minimum,
                }
                .into());
            }
            Ok(Share {
                minimum: u64::try_from(minimum)?,
                maximum: u64::try_from(maximum.min(u128::from(target)))?,
                weight: members
                    .iter()
                    .map(|index| inputs[*index].weight)
                    .max()
                    .unwrap_or(1),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let minimum = shares
        .iter()
        .map(|share| u128::from(share.minimum))
        .sum::<u128>();
    if minimum > u128::from(target) {
        return Err(InsufficientBitrateBudget {
            target,
            required: minimum,
        }
        .into());
    }
    let distributable = (target - target / 20).max(u64::try_from(minimum)?);
    let group_rates = waterfill(distributable, &shares)?;
    let mut ideal = vec![0; inputs.len()];
    for (members, group_rate) in groups.values().zip(group_rates) {
        let shares = members
            .iter()
            .map(|index| {
                Ok(Share {
                    minimum: inputs[*index].limits.minimum(),
                    maximum: inputs[*index].limits.maximum(),
                    weight: inputs[*index]
                        .pixels
                        .checked_mul(u64::from(inputs[*index].area_weight))
                        .context("weighted bitrate area overflow")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        for (index, rate) in members.iter().zip(waterfill(group_rate, &shares)?) {
            let rounded = if rate < RATE_STEP {
                rate
            } else {
                rate / RATE_STEP * RATE_STEP
            };
            ideal[*index] = rounded.max(inputs[*index].limits.minimum());
        }
    }
    let mut rates = inputs
        .iter()
        .zip(&ideal)
        .map(|(input, ideal)| input.current.unwrap_or(*ideal))
        .collect::<Vec<_>>();
    let mut total = rates.iter().map(|rate| u128::from(*rate)).sum::<u128>();
    let increases = rates
        .iter()
        .zip(&ideal)
        .map(|(current, ideal)| {
            let extra = ideal.saturating_sub(*current);
            if extra >= RATE_STEP.max(current.div_ceil(4)) {
                u128::from(extra)
            } else {
                0
            }
        })
        .sum::<u128>();
    // Free room for meaningful increases even when the old total still fits.
    // Ignore other tiny downward adjustments that fit in spare headroom, avoiding
    // parent keyframes for tiny popups.
    let mut donors = (0..inputs.len())
        .filter(|index| rates[*index] > ideal[*index])
        .collect::<Vec<_>>();
    donors.sort_by_key(|index| {
        (
            std::cmp::Reverse(rates[*index] - ideal[*index]),
            inputs[*index].key,
        )
    });
    for index in donors {
        if total + increases <= u128::from(target) {
            break;
        }
        // This donor already pays for an encoder replacement. Move fully to its
        // ideal now, restoring room for later small streams at no extra switch.
        let reduction = rates[index] - ideal[index];
        rates[index] = ideal[index];
        total -= u128::from(reduction);
    }
    ensure!(
        total + increases <= u128::from(target),
        "shared bitrate targets could not fit"
    );
    for (index, input) in inputs.iter().enumerate() {
        let increase = ideal[index].saturating_sub(rates[index]);
        if increase >= RATE_STEP.max(rates[index].div_ceil(4))
            && total + u128::from(increase) <= u128::from(target)
        {
            rates[index] = ideal[index];
            total += u128::from(increase);
        }
        input
            .limits
            .validate(rates[index])
            .context("invalid shared allocation")?;
    }
    Ok(rates)
}
