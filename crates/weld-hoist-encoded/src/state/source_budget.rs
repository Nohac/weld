//! Register a complete scheduled layer inventory before freezing any frame rate.

use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, time::Instant};
use weld_client::{
    ClientBufferMetadata, ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceId,
    SurfaceBufferChange, SurfaceLayerId,
};
use weld_media::{MediaStreamId, StreamGeneration};

use super::{EncodedSourceState, SourceStream, take_counter};
use crate::budget::StreamDemand;

#[derive(Default)]
struct GroupRates {
    streams: usize,
    pixels: u64,
    requested: u64,
    applied: u64,
    pending: usize,
}

impl EncodedSourceState {
    pub(super) fn prepare_streams(&mut self, event: &ClientSurfaceEvent) -> Result<()> {
        let now = Instant::now();
        let ClientSurfaceEventKind::Commit(commit) = &event.kind else {
            return self.update_budget(None, now);
        };
        let mut replacements = Vec::new();
        for update in &commit.buffers {
            if let SurfaceBufferChange::Replaced { buffer, .. } = &update.change {
                let metadata = buffer.metadata();
                ensure!(
                    metadata.extent.width > 0 && metadata.extent.height > 0,
                    "encoded extent is zero"
                );
                replacements.push((update.layer, metadata));
            }
        }
        let new_streams = replacements
            .iter()
            .filter(|(layer, _)| !self.streams.contains_key(&(event.surface, *layer)))
            .count();
        let retained_streams = self
            .streams
            .keys()
            .filter(|(surface, layer)| {
                *surface != event.surface
                    || commit.buffers.iter().any(|update| {
                        update.layer == *layer
                            && !matches!(update.change, SurfaceBufferChange::Removed)
                    })
            })
            .count();
        if let Some(budget) = &self.budget {
            budget.preflight(
                retained_streams
                    .checked_add(new_streams)
                    .context("stream count overflow")?,
                now,
            )?;
        }
        self.reconcile_streams(event)?;
        for (layer, metadata) in replacements {
            self.register_stream(event.surface, layer, metadata)?;
        }
        self.update_budget(Some(event), now)
    }

    pub(super) fn register_stream(
        &mut self,
        surface: ClientSurfaceId,
        layer: SurfaceLayerId,
        metadata: ClientBufferMetadata,
    ) -> Result<()> {
        if self.streams.contains_key(&(surface, layer)) {
            return Ok(());
        }
        let stream = MediaStreamId::new(take_counter(&mut self.next_stream, "encoded stream")?);
        let frozen_rate = self
            .rates
            .as_ref()
            .and_then(|rates| rates.register(stream, surface, layer));
        if self.budget.is_some() {
            ensure!(
                frozen_rate.is_some(),
                "managed encoder rate registry is unavailable"
            );
        }
        self.streams.insert(
            (surface, layer),
            SourceStream {
                stream,
                generation: StreamGeneration::new(1),
                visible_extent: (metadata.extent.width, metadata.extent.height),
                next_sequence: Some(0),
                frozen_rate,
                frozen_frame_rate: None,
            },
        );
        Ok(())
    }

    pub(super) fn update_budget(
        &mut self,
        event: Option<&ClientSurfaceEvent>,
        now: Instant,
    ) -> Result<()> {
        let Some(budget) = &self.budget else {
            return Ok(());
        };
        let mut demands = self
            .streams
            .iter()
            .map(|((surface, layer), stream)| {
                let replacement_extent =
                    event
                        .filter(|event| event.surface == *surface)
                        .and_then(|event| {
                            let ClientSurfaceEventKind::Commit(commit) = &event.kind else {
                                return None;
                            };
                            commit.buffers.iter().find_map(|update| {
                                if update.layer != *layer {
                                    return None;
                                }
                                match &update.change {
                                    SurfaceBufferChange::Replaced { buffer, .. } => {
                                        Some(buffer.metadata().extent)
                                    }
                                    _ => None,
                                }
                            })
                        });
                let (width, height) = replacement_extent.map_or(stream.visible_extent, |extent| {
                    (extent.width, extent.height)
                });
                Ok(StreamDemand {
                    stream: stream.stream,
                    group: self
                        .activity
                        .group(*surface)
                        .context("budgeted stream has no owning hoist group")?,
                    pixels: u64::from(width) * u64::from(height),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        demands.sort_by_key(|demand| demand.stream);
        self.activity
            .snapshot(self.policy, &mut self.budget_activity);
        budget.update_inventory(demands)?;
        budget.update_attention(&self.budget_activity.attention, now)
    }

    pub(super) fn refresh_budget_attention(&mut self, now: Instant) -> Result<()> {
        let Some(budget) = &self.budget else {
            return Ok(());
        };
        self.activity
            .snapshot(self.policy, &mut self.budget_activity);
        budget.update_attention(&self.budget_activity.attention, now)
    }

    pub(super) fn report_budget(&self) {
        let (Some(budget), Some(rates)) = (&self.budget, &self.rates) else {
            return;
        };
        let Ok(streams) = rates.control().streams() else {
            return;
        };
        let mut groups = BTreeMap::new();
        for status in streams {
            let Some(group) = self.activity.group(status.surface) else {
                continue;
            };
            let value = groups.entry(group).or_insert_with(GroupRates::default);
            value.streams += 1;
            if let Some(stream) = self.streams.get(&(status.surface, status.layer)) {
                value.pixels = value.pixels.saturating_add(
                    u64::from(stream.visible_extent.0) * u64::from(stream.visible_extent.1),
                );
            }
            value.requested = value
                .requested
                .saturating_add(status.requested.bits_per_second);
            if let Some(applied) = status.applied {
                value.applied = value
                    .applied
                    .saturating_add(applied.request.bits_per_second);
            }
            value.pending +=
                usize::from(status.applied.map(|value| value.request) != Some(status.requested));
        }
        for (group, value) in groups {
            tracing::debug!(target: "weld_media_diag", session = ?group.session, root = ?group.root,
                allocation_priority = ?budget.priority(group), streams = value.streams,
                input_pixels = value.pixels, requested_bitrate = value.requested,
                applied_bitrate = value.applied, pending_streams = value.pending,
                "encoded window bitrate");
        }
    }
}
