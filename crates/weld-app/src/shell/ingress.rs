//! Latest unobserved commits for one presenter, bounded by ordered control events.

use std::collections::HashMap;

use weld_client::{ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceId};

/// Buffer leases coalesce before image preparation. An intervening control or
/// mapping transition closes the coalescing segment so lifecycle order survives.
#[derive(Default)]
pub(super) struct PresentationIngress {
    events: Vec<ClientSurfaceEvent>,
    commits: HashMap<ClientSurfaceId, usize>,
}

impl PresentationIngress {
    pub(super) fn push(&mut self, mut event: ClientSurfaceEvent) {
        if let ClientSurfaceEventKind::Commit(current) = &mut event.kind {
            if let Some(index) = self.commits.get(&event.surface).copied()
                && let Some(previous_event) = self.events.get_mut(index)
                && let ClientSurfaceEventKind::Commit(previous) = &mut previous_event.kind
            {
                if previous.mapped == current.mapped {
                    current.carry_unobserved_content_from(previous);
                    *previous_event = event;
                    return;
                }
                self.commits.clear();
            }
            self.commits.insert(event.surface, self.events.len());
        } else {
            self.commits.clear();
        }
        self.events.push(event);
    }

    pub(super) fn drain(&mut self) -> impl Iterator<Item = ClientSurfaceEvent> + '_ {
        self.commits.clear();
        self.events.drain(..)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use weld_client::{
        ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId,
        ClientCommitRevision, ClientSurfaceCommit, Extent, SurfaceBufferChange,
        SurfaceBufferUpdate, SurfaceLayerId,
    };

    use super::*;

    fn commit(surface: u64, revision: u64, mapped: bool) -> ClientSurfaceEvent {
        ClientSurfaceEvent {
            surface: ClientSurfaceId::for_test(surface),
            kind: ClientSurfaceEventKind::Commit(ClientSurfaceCommit {
                revision: ClientCommitRevision::new(revision),
                alpha_mode: Default::default(),
                mapped,
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: Vec::new(),
            }),
        }
    }

    fn with_buffer(mut event: ClientSurfaceEvent, completed: &Rc<Cell<u32>>) -> ClientSurfaceEvent {
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        let source = event.surface.source();
        let completed = completed.clone();
        let buffer = ClientBufferLease::new(
            ClientBufferId::new(source, 1),
            ClientBufferUseId::new(source, 1),
            metadata,
            Rc::new(()),
            move |_| completed.set(completed.get() + 1),
        )
        .expect("matching buffer source");
        if let ClientSurfaceEventKind::Commit(commit) = &mut event.kind {
            commit.buffers.push(SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(1),
                change: SurfaceBufferChange::Replaced { metadata, buffer },
            });
        }
        event
    }

    #[test]
    fn interleaved_surfaces_release_superseded_uses_before_presentation() {
        let completed = Rc::new(Cell::new(0));
        let mut pending = PresentationIngress::default();
        for revision in 1..=100 {
            for surface in 1..=3 {
                pending.push(with_buffer(commit(surface, revision, true), &completed));
            }
        }
        assert_eq!(pending.events.len(), 3);
        assert_eq!(completed.get(), 297);
        for event in pending.drain() {
            let ClientSurfaceEventKind::Commit(commit) = event.kind else {
                panic!("expected latest commit");
            };
            assert_eq!(commit.revision, ClientCommitRevision::new(100));
        }
        assert_eq!(completed.get(), 300);
        pending.push(commit(1, 101, true));
        assert_eq!(pending.drain().count(), 1, "drain clears the index");
    }

    #[test]
    fn retained_layers_keep_the_latest_unobserved_content() {
        let completed = Rc::new(Cell::new(0));
        let mut pending = PresentationIngress::default();
        pending.push(with_buffer(commit(1, 1, true), &completed));
        pending.push(commit(2, 1, true));
        let mut retained = commit(1, 2, true);
        if let ClientSurfaceEventKind::Commit(commit) = &mut retained.kind {
            commit.buffers.push(SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(1),
                change: SurfaceBufferChange::Retained {
                    metadata: ClientBufferMetadata::new(Extent::new(1, 1), true),
                },
            });
        }
        pending.push(retained);
        assert_eq!(completed.get(), 0);
        let events = pending.drain().collect::<Vec<_>>();
        let ClientSurfaceEventKind::Commit(commit) = &events[0].kind else {
            panic!("expected commit");
        };
        assert!(matches!(
            commit.buffers[0].change,
            SurfaceBufferChange::Replaced { .. }
        ));
        drop(events);
        assert_eq!(completed.get(), 1);
    }

    #[test]
    fn unmaps_and_controls_preserve_order_across_surfaces() {
        let mut pending = PresentationIngress::default();
        pending.push(commit(1, 1, true));
        pending.push(commit(2, 1, true));
        pending.push(commit(1, 2, false));
        pending.push(commit(2, 2, true));
        pending.push(commit(1, 3, true));
        pending.push(ClientSurfaceEvent {
            surface: ClientSurfaceId::for_test(1),
            kind: ClientSurfaceEventKind::Destroyed,
        });
        pending.push(commit(2, 3, true));
        assert_eq!(pending.drain().count(), 7);
    }
}
