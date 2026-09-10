//! Register a complete scheduled layer inventory before freezing any frame rate.

use anyhow::{Context, Result, ensure};
use weld_client::{
    ClientBufferMetadata, ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceId,
    SurfaceBufferChange, SurfaceLayerId,
};
use weld_media::{MediaStreamId, StreamGeneration};

use super::{EncodedSourceState, SourceStream, take_counter};
use crate::budget::StreamDemand;

impl EncodedSourceState {
    pub(super) fn prepare_streams(&mut self, event: &ClientSurfaceEvent) -> Result<()> {
        let ClientSurfaceEventKind::Commit(commit) = &event.kind else {
            return self.update_budget(None);
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
            )?;
        }
        self.reconcile_streams(event)?;
        for (layer, metadata) in replacements {
            self.register_stream(event.surface, layer, metadata)?;
        }
        self.update_budget(Some(event))
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
            },
        );
        Ok(())
    }

    pub(super) fn update_budget(&self, event: Option<&ClientSurfaceEvent>) -> Result<()> {
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
        budget.update(demands)
    }
}
