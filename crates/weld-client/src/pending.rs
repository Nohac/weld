//! Consumer-owned pending surface state, shared by presentation and media admission.

use std::collections::VecDeque;

use crate::{ClientSurfaceEvent, ClientSurfaceEventKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingEventUpdate {
    Queued,
    CommitCoalesced,
    MetadataReplaced,
}

/// Latest unconsumed commits between ordered lifecycle/control records.
///
/// Each consumer owns a queue and advances it at its own readiness/cadence.
/// `Scope` separates caller-owned generations such as hoist sessions. Metadata
/// is independently coalesced and does not obstruct buffer-history merging.
/// Dropping superseded snapshots releases only this consumer's buffer references.
pub struct PendingClientEvents<Scope = ()> {
    events: VecDeque<PendingEvent<Scope>>,
}

struct PendingEvent<Scope> {
    record: (Scope, ClientSurfaceEvent),
    barrier: bool,
}

#[derive(Default)]
struct Insertion {
    replacement: Option<usize>,
    barrier: bool,
}

impl<Scope> Default for PendingClientEvents<Scope> {
    fn default() -> Self {
        Self {
            events: VecDeque::new(),
        }
    }
}

impl<Scope: Eq> PendingClientEvents<Scope> {
    /// Append an event or replace its superseded pending state. Controls and
    /// mapping/scope transitions preserve ordering; snapshots remain atomic.
    pub fn push(&mut self, scope: Scope, event: ClientSurfaceEvent) -> PendingEventUpdate {
        let insertion = self.plan(&scope, &event);
        self.insert(scope, event, insertion)
    }

    /// Enforce a caller's remaining event budget without changing pending state
    /// on rejection. Replacements remain admissible at capacity.
    pub fn try_push(
        &mut self,
        scope: Scope,
        event: ClientSurfaceEvent,
        limit: usize,
    ) -> Result<PendingEventUpdate, (Scope, ClientSurfaceEvent)> {
        let insertion = self.plan(&scope, &event);
        if insertion.replacement.is_none() && self.len() >= limit {
            return Err((scope, event));
        }
        Ok(self.insert(scope, event, insertion))
    }

    fn plan(&self, scope: &Scope, event: &ClientSurfaceEvent) -> Insertion {
        match &event.kind {
            ClientSurfaceEventKind::Commit(current) => {
                for index in (0..self.events.len()).rev() {
                    let pending = &self.events[index];
                    let (previous_scope, previous) = &pending.record;
                    match &previous.kind {
                        ClientSurfaceEventKind::Metadata(_) => continue,
                        ClientSurfaceEventKind::Commit(previous_commit) => {
                            if previous.surface == event.surface {
                                let compatible = previous_scope == scope
                                    && previous_commit.mapped == current.mapped;
                                return Insertion {
                                    replacement: compatible.then_some(index),
                                    barrier: !compatible || pending.barrier,
                                };
                            }
                            if pending.barrier {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                return Insertion {
                    replacement: None,
                    barrier: !current.mapped,
                };
            }
            ClientSurfaceEventKind::Metadata(_) => {
                for (index, pending) in self.events.iter().enumerate().rev() {
                    let (previous_scope, previous) = &pending.record;
                    if previous_scope == scope
                        && previous.surface == event.surface
                        && matches!(previous.kind, ClientSurfaceEventKind::Metadata(_))
                    {
                        return Insertion {
                            replacement: Some(index),
                            barrier: false,
                        };
                    }
                    if pending.barrier
                        || !matches!(
                            previous.kind,
                            ClientSurfaceEventKind::Metadata(_) | ClientSurfaceEventKind::Commit(_)
                        )
                    {
                        break;
                    }
                }
            }
            _ => {}
        }
        Insertion::default()
    }

    fn insert(
        &mut self,
        scope: Scope,
        mut event: ClientSurfaceEvent,
        insertion: Insertion,
    ) -> PendingEventUpdate {
        if let Some(index) = insertion.replacement
            && let Some(previous) = self.events.get_mut(index)
            && previous.barrier
            && let (ClientSurfaceEventKind::Commit(current), ClientSurfaceEventKind::Commit(old)) =
                (&mut event.kind, &previous.record.1.kind)
        {
            current.carry_unobserved_content_from(old);
            previous.record = (scope, event);
            return PendingEventUpdate::CommitCoalesced;
        }
        let mut outcome = PendingEventUpdate::Queued;
        if let Some(index) = insertion.replacement
            && let Some(previous) = self.events.remove(index)
        {
            outcome = PendingEventUpdate::MetadataReplaced;
            if let (
                ClientSurfaceEventKind::Commit(current),
                ClientSurfaceEventKind::Commit(previous),
            ) = (&mut event.kind, &previous.record.1.kind)
            {
                current.carry_unobserved_content_from(previous);
                outcome = PendingEventUpdate::CommitCoalesced;
            }
        }
        self.events.push_back(PendingEvent {
            record: (scope, event),
            barrier: insertion.barrier,
        });
        outcome
    }
}

impl<Scope> PendingClientEvents<Scope> {
    pub fn len(&self) -> usize {
        self.events.len()
    }
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
    pub fn front(&self) -> Option<&(Scope, ClientSurfaceEvent)> {
        self.events.front().map(|event| &event.record)
    }
    pub fn pop_front(&mut self) -> Option<(Scope, ClientSurfaceEvent)> {
        self.events.pop_front().map(|event| event.record)
    }
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &(Scope, ClientSurfaceEvent)> {
        self.events.iter().map(|event| &event.record)
    }
    pub fn drain(&mut self) -> impl Iterator<Item = (Scope, ClientSurfaceEvent)> + '_ {
        self.events.drain(..).map(|event| event.record)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use crate::{
        ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId,
        ClientCommitRevision, ClientSurfaceCommit, Extent, SurfaceBufferChange,
        SurfaceBufferUpdate, SurfaceLayerId,
    };

    use super::*;
    use crate::ClientSurfaceId;

    fn commit(surface: u64, revision: u64, mapped: bool) -> ClientSurfaceEvent {
        ClientSurfaceEvent {
            surface: ClientSurfaceId::for_test(surface),
            kind: ClientSurfaceEventKind::Commit(ClientSurfaceCommit::from(
                crate::ClientSurfaceState {
                    revision: ClientCommitRevision::new(revision),
                    alpha_mode: Default::default(),
                    mapped,
                    root: None,
                    window_geometry: None,
                    overlays: Vec::new(),
                    inputs: Vec::new(),
                    buffers: Vec::new(),
                },
            )),
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
            commit.make_mut().buffers.push(SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(1),
                change: SurfaceBufferChange::Replaced { metadata, buffer },
            });
        }
        event
    }

    #[test]
    fn interleaved_surfaces_release_superseded_uses_before_presentation() {
        let completed = Rc::new(Cell::new(0));
        let mut pending = PendingClientEvents::default();
        for revision in 1..=100 {
            for surface in 1..=3 {
                pending.push((), with_buffer(commit(surface, revision, true), &completed));
            }
        }
        assert_eq!(pending.events.len(), 3);
        assert_eq!(completed.get(), 297);
        for (_, event) in pending.drain() {
            let ClientSurfaceEventKind::Commit(commit) = event.kind else {
                panic!("expected latest commit");
            };
            assert_eq!(commit.revision, ClientCommitRevision::new(100));
        }
        assert_eq!(completed.get(), 300);
        pending.push((), commit(1, 101, true));
        assert_eq!(pending.drain().count(), 1);
    }

    #[test]
    fn retained_layers_keep_the_latest_unobserved_content() {
        let completed = Rc::new(Cell::new(0));
        let mut pending = PendingClientEvents::default();
        pending.push((), with_buffer(commit(1, 1, true), &completed));
        pending.push((), commit(2, 1, true));
        let mut retained = commit(1, 2, true);
        if let ClientSurfaceEventKind::Commit(commit) = &mut retained.kind {
            commit.make_mut().buffers.push(SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(1),
                change: SurfaceBufferChange::Retained {
                    metadata: ClientBufferMetadata::new(Extent::new(1, 1), true),
                },
            });
        }
        pending.push((), retained);
        assert_eq!(completed.get(), 0);
        let events = pending.drain().map(|(_, event)| event).collect::<Vec<_>>();
        let event = events
            .iter()
            .find(|event| event.surface == ClientSurfaceId::for_test(1))
            .expect("first surface");
        let ClientSurfaceEventKind::Commit(commit) = &event.kind else {
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
        let mut pending = PendingClientEvents::default();
        pending.push((), commit(1, 1, true));
        pending.push((), commit(2, 1, true));
        pending.push((), commit(1, 2, false));
        pending.push((), commit(2, 2, true));
        pending.push((), commit(1, 3, true));
        pending.push(
            (),
            ClientSurfaceEvent {
                surface: ClientSurfaceId::for_test(1),
                kind: ClientSurfaceEventKind::Destroyed,
            },
        );
        pending.push((), commit(2, 3, true));
        assert_eq!(pending.drain().count(), 7);
    }

    #[test]
    fn independently_paced_consumers_retain_their_own_buffer_uses() {
        let completed = Rc::new(Cell::new(0));
        let initial = with_buffer(commit(1, 1, true), &completed);
        let mut presenter = PendingClientEvents::default();
        let mut encoder = PendingClientEvents::default();
        presenter.push((), initial.clone());
        encoder.push((), initial);
        let presented = presenter.pop_front().expect("presented frame");
        presenter.push((), with_buffer(commit(1, 2, true), &completed));
        encoder.push((), commit(1, 2, true));
        assert_eq!(completed.get(), 0, "presenter still owns the first use");
        drop(presented);
        assert_eq!(completed.get(), 1);
        drop(encoder);
        assert_eq!(completed.get(), 1, "second use belongs to presenter");
        drop(presenter);
        assert_eq!(completed.get(), 2);
    }

    #[test]
    fn retained_merge_does_not_mutate_another_consumers_snapshot() {
        let completed = Rc::new(Cell::new(0));
        let mut pending = PendingClientEvents::default();
        pending.push((), with_buffer(commit(1, 1, true), &completed));
        let mut retained = commit(1, 2, true);
        let ClientSurfaceEventKind::Commit(state) = &mut retained.kind else {
            panic!("commit fixture");
        };
        state.make_mut().buffers.push(SurfaceBufferUpdate {
            layer: SurfaceLayerId::new(1),
            change: SurfaceBufferChange::Retained {
                metadata: ClientBufferMetadata::new(Extent::new(1, 1), true),
            },
        });
        let other_consumer = retained.clone();
        pending.push((), retained);
        let ClientSurfaceEventKind::Commit(other) = other_consumer.kind else {
            panic!("commit fixture");
        };
        assert!(matches!(
            other.buffers[0].change,
            SurfaceBufferChange::Retained { .. }
        ));
        let (_, event) = pending.pop_front().expect("merged frame");
        let ClientSurfaceEventKind::Commit(merged) = event.kind else {
            panic!("commit fixture");
        };
        assert!(matches!(
            merged.buffers[0].change,
            SurfaceBufferChange::Replaced { .. }
        ));
        assert_eq!(completed.get(), 0);
        drop(merged);
        assert_eq!(completed.get(), 1);
    }

    #[test]
    fn capacity_allows_replacement_but_rejects_new_scope_without_mutating_queue() {
        let completed = Rc::new(Cell::new(0));
        let mut pending = PendingClientEvents::default();
        pending
            .try_push(1, with_buffer(commit(1, 1, true), &completed), 1)
            .expect("first");
        assert!(pending.try_push(2, commit(1, 2, true), 1).is_err());
        assert_eq!(completed.get(), 0);
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending
                .try_push(1, with_buffer(commit(1, 3, true), &completed), 1)
                .expect("replace"),
            PendingEventUpdate::CommitCoalesced
        );
        assert_eq!(completed.get(), 1);
        assert_eq!(pending.len(), 1);
        pending.push(2, commit(1, 4, true));
        assert_eq!(pending.len(), 2, "session transition remains ordered");
    }

    #[test]
    fn coalescing_a_mapping_barrier_does_not_move_it_past_other_surfaces() {
        let mut pending = PendingClientEvents::default();
        for (surface, revision, mapped) in [
            (1, 1, true),
            (1, 2, false),
            (2, 1, true),
            (1, 3, false),
            (1, 4, true),
            (2, 2, true),
            (1, 5, true),
        ] {
            pending.push((), commit(surface, revision, mapped));
        }
        let revisions: Vec<_> = pending
            .drain()
            .map(|(_, event)| {
                let ClientSurfaceEventKind::Commit(commit) = event.kind else {
                    panic!("commit fixture");
                };
                commit.revision.raw()
            })
            .collect();
        assert_eq!(revisions, [1, 3, 1, 5, 2]);
    }

    #[test]
    fn labels_do_not_cross_destroyed_or_interaction_records() {
        let mut pending = PendingClientEvents::default();
        let metadata = |title: &str| ClientSurfaceEvent {
            surface: ClientSurfaceId::for_test(1),
            kind: ClientSurfaceEventKind::Metadata(
                crate::ClientSurfaceMetadata::new("app".into(), title.into()).expect("metadata"),
            ),
        };
        pending.push((), metadata("old"));
        pending.push(
            (),
            ClientSurfaceEvent {
                surface: ClientSurfaceId::for_test(1),
                kind: ClientSurfaceEventKind::Destroyed,
            },
        );
        pending.push((), metadata("new"));
        assert_eq!(pending.len(), 3);
    }
}
