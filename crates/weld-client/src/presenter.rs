//! Surface lifecycle barriers and bounded frame handoff before GPU import.

use crate::pending_order::{self, Insertion, Kind};
use crate::{
    ClientEventSink, ClientSourceId, ClientSurfaceCommit, ClientSurfaceEvent,
    ClientSurfaceEventKind, ClientSurfaceId, PresentationMailbox, PresentationQueueStats,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

/// Work needed by a presenter after receiving a surface event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationDemand {
    /// An ordinary content update needs one presentation opportunity.
    Content,
    /// Mapping or role changes also need layout and resource preparation.
    Structure,
}

/// Ordered lifecycle records and per-surface frame selection. Local surfaces
/// retain their latest commit; asynchronous sources may enable one jitter slot.
/// Consumers call `drain_ready` once per presentation opportunity and request
/// another opportunity while `has_pending` is true.
#[derive(Default)]
pub struct ClientPresentationInbox {
    mapped: HashSet<ClientSurfaceId>,
    pending: VecDeque<Record>,
    intervals: HashMap<ClientSourceId, Duration>,
    completed: HashMap<ClientSourceId, PresentationQueueStats>,
}

enum Record {
    Event(ClientSurfaceEvent),
    Frames {
        surface: ClientSurfaceId,
        queue: PresentationMailbox<ClientSurfaceCommit>,
        barrier: bool,
    },
}

impl ClientEventSink for ClientPresentationInbox {
    fn push_event(&mut self, event: ClientSurfaceEvent) {
        self.push(event);
    }
}

impl ClientPresentationInbox {
    /// Use the presenter's nominal interval to bound an asynchronous source's
    /// jitter history. Already queued frames adopt the new age limit.
    pub fn smooth_source(&mut self, source: ClientSourceId, interval: Duration) {
        self.intervals.insert(source, interval);
        for record in &mut self.pending {
            if let Record::Frames { surface, queue, .. } = record
                && surface.source() == source
            {
                queue.enable_smoothing(interval);
            }
        }
    }

    /// Refresh the age bound for sources already using asynchronous handoff.
    pub fn set_interval(&mut self, interval: Duration) {
        for value in self.intervals.values_mut() {
            *value = interval;
        }
        for record in &mut self.pending {
            if let Record::Frames { queue, .. } = record {
                queue.set_interval(interval);
            }
        }
    }

    pub fn push(&mut self, event: ClientSurfaceEvent) -> PresentationDemand {
        self.push_at(event, Instant::now())
    }

    pub fn push_at(&mut self, event: ClientSurfaceEvent, now: Instant) -> PresentationDemand {
        let surface = event.surface;
        let demand = match &event.kind {
            ClientSurfaceEventKind::Commit(commit) => {
                let changed = if commit.mapped {
                    self.mapped.insert(surface)
                } else {
                    self.mapped.remove(&surface)
                };
                if changed {
                    PresentationDemand::Structure
                } else {
                    PresentationDemand::Content
                }
            }
            ClientSurfaceEventKind::Destroyed => {
                self.mapped.remove(&surface);
                PresentationDemand::Structure
            }
            ClientSurfaceEventKind::Role(_) => PresentationDemand::Structure,
            _ => PresentationDemand::Content,
        };
        match event.kind {
            ClientSurfaceEventKind::Commit(commit) => self.commit(surface, commit, now),
            ClientSurfaceEventKind::Metadata(_) => {
                // Labels can coalesce through content updates, but never through
                // mapping transitions or controls.
                let replacement = self.plan(surface, Kind::Metadata).replacement;
                if let Some(index) = replacement {
                    self.pending.remove(index);
                }
                self.pending.push_back(Record::Event(event));
            }
            _ => {
                self.collapse_history(now);
                self.pending.push_back(Record::Event(event));
            }
        }
        demand
    }

    fn commit(&mut self, surface: ClientSurfaceId, mut commit: ClientSurfaceCommit, now: Instant) {
        let Insertion {
            replacement,
            barrier,
        } = self.plan(
            surface,
            Kind::Commit {
                mapped: commit.mapped,
            },
        );
        if let Some(index) = replacement
            && let Some(Record::Frames { queue, .. }) = self.pending.get(index)
            && let Some(previous) = queue.newest()
        {
            commit.carry_unobserved_content_from(previous);
        }
        if let Some(index) = replacement {
            // Mapping barriers retain their original position across other
            // surfaces. Ordinary content follows latest arrival order.
            if barrier {
                if let Some(Record::Frames { queue, .. }) = self.pending.get_mut(index) {
                    enqueue_commit(queue, commit, now);
                }
            } else if let Some(Record::Frames { mut queue, .. }) = self.pending.remove(index) {
                enqueue_commit(&mut queue, commit, now);
                self.pending.push_back(Record::Frames {
                    surface,
                    queue,
                    barrier,
                });
            }
            return;
        }
        if barrier {
            self.collapse_history(now);
        }
        let mut queue = self
            .intervals
            .get(&surface.source())
            .map_or_else(PresentationMailbox::default, |interval| {
                PresentationMailbox::smoothing(*interval)
            });
        queue.push(commit, now);
        self.pending.push_back(Record::Frames {
            surface,
            queue,
            barrier,
        });
    }

    fn plan(&self, surface: ClientSurfaceId, kind: Kind) -> Insertion {
        pending_order::plan(
            surface,
            kind,
            self.pending.iter().enumerate().map(|(index, record)| {
                let (surface, kind, barrier) = match record {
                    Record::Event(event) => (event.surface, Kind::of(event), false),
                    Record::Frames {
                        surface,
                        queue,
                        barrier,
                    } => (
                        *surface,
                        Kind::Commit {
                            mapped: queue.newest().is_some_and(|commit| commit.mapped),
                        },
                        *barrier,
                    ),
                };
                (
                    index,
                    pending_order::Record {
                        surface,
                        kind,
                        barrier,
                        same_scope: true,
                    },
                )
            }),
        )
    }

    fn collapse_history(&mut self, _now: Instant) {
        // A control consumes the latest preceding state in order. Older jitter
        // slots must never reappear after unmap, destruction, or interaction.
        for record in &mut self.pending {
            if let Record::Frames { queue, .. } = record {
                queue.keep_latest();
            }
        }
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn stats(&self) -> PresentationQueueStats {
        let mut result = PresentationQueueStats::default();
        for stats in self.completed.values() {
            result += *stats;
        }
        for record in &self.pending {
            if let Record::Frames { queue, .. } = record {
                result += queue.stats();
            }
        }
        result
    }

    pub fn stats_for_source(&self, source: ClientSourceId) -> PresentationQueueStats {
        let mut result = self.completed.get(&source).copied().unwrap_or_default();
        for record in &self.pending {
            if let Record::Frames { surface, queue, .. } = record
                && surface.source() == source
            {
                result += queue.stats();
            }
        }
        result
    }

    /// One selection per surface segment at this display opportunity.
    pub fn drain_ready(&mut self, now: Instant) -> impl Iterator<Item = ClientSurfaceEvent> + '_ {
        let count = self.pending.len();
        (0..count).filter_map(move |_| self.pop_at(now))
    }

    pub fn pop_front(&mut self) -> Option<ClientSurfaceEvent> {
        self.pop_at(Instant::now())
    }

    fn pop_at(&mut self, now: Instant) -> Option<ClientSurfaceEvent> {
        match self.pending.pop_front()? {
            Record::Event(event) => Some(event),
            Record::Frames {
                surface,
                mut queue,
                barrier,
            } => {
                let (commit, _, discarded) = queue.pop(now);
                drop(discarded);
                if !queue.is_empty() {
                    self.pending.push_back(Record::Frames {
                        surface,
                        queue,
                        barrier,
                    });
                } else {
                    *self.completed.entry(surface.source()).or_default() += queue.stats();
                }
                commit.map(|commit| ClientSurfaceEvent {
                    surface,
                    kind: ClientSurfaceEventKind::Commit(commit),
                })
            }
        }
    }

    pub fn drain(&mut self) -> impl Iterator<Item = ClientSurfaceEvent> + '_ {
        self.drain_ready(Instant::now())
    }
}

fn enqueue_commit(
    queue: &mut PresentationMailbox<ClientSurfaceCommit>,
    commit: ClientSurfaceCommit,
    now: Instant,
) {
    // Geometry and buffer inventory form one atomic display transaction.
    // A resize/crop/layout change replaces prior jitter history immediately.
    if queue
        .newest()
        .is_some_and(|previous| !same_layout(previous, &commit))
    {
        queue.clear();
    }
    queue.push(commit, now);
}

fn same_layout(previous: &ClientSurfaceCommit, current: &ClientSurfaceCommit) -> bool {
    previous.mapped == current.mapped
        && previous.alpha_mode == current.alpha_mode
        && previous.root == current.root
        && previous.window_geometry == current.window_geometry
        && previous.overlays == current.overlays
        && previous.inputs == current.inputs
        && previous.buffers.len() == current.buffers.len()
        && previous
            .buffers
            .iter()
            .zip(&current.buffers)
            .all(|(old, new)| {
                old.layer == new.layer && old.change.metadata() == new.change.metadata()
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientCommitRevision, ClientId, ClientSourceId, ClientSurfaceState};

    fn event(mapped: bool, revision: u64) -> ClientSurfaceEvent {
        ClientSurfaceEvent {
            surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1),
            kind: ClientSurfaceEventKind::Commit(
                ClientSurfaceState {
                    revision: ClientCommitRevision::new(revision),
                    mapped,
                    alpha_mode: Default::default(),
                    root: None,
                    window_geometry: None,
                    overlays: Vec::new(),
                    inputs: Vec::new(),
                    buffers: Vec::new(),
                }
                .into(),
            ),
        }
    }

    fn revisions(events: impl Iterator<Item = ClientSurfaceEvent>) -> Vec<u64> {
        events
            .map(|event| match event.kind {
                ClientSurfaceEventKind::Commit(commit) => commit.revision.raw(),
                _ => 0,
            })
            .collect()
    }

    #[test]
    fn source_statistics_stay_separate_across_queue_drains() {
        let first = ClientSourceId::new(1);
        let second = ClientSourceId::new(2);
        let mut inbox = ClientPresentationInbox::default();
        let now = Instant::now();
        inbox.push_at(event(true, 1), now);
        inbox.push_at(event(true, 2), now);
        let mut other = event(true, 1);
        other.surface = ClientSurfaceId::new(ClientId::new(second, 1), 1);
        inbox.push_at(other, now);
        assert_eq!(inbox.stats_for_source(first).superseded, 1);
        assert_eq!(inbox.stats_for_source(second).superseded, 0);
        assert_eq!(inbox.drain_ready(now).count(), 2);
        assert_eq!(inbox.stats_for_source(first).superseded, 1);
        assert_eq!(inbox.stats_for_source(second).superseded, 0);
        assert_eq!(inbox.stats_for_source(first).selected, 1);
        assert_eq!(inbox.stats_for_source(second).selected, 1);
        assert_eq!(inbox.stats().selected, 2);
    }

    #[test]
    fn remote_jitter_is_selected_once_per_opportunity_and_final_frame_is_kept() {
        let now = Instant::now();
        let mut inbox = ClientPresentationInbox::default();
        inbox.smooth_source(ClientSourceId::new(1), Duration::from_millis(11));
        inbox.push_at(event(true, 1), now);
        inbox.push_at(event(true, 2), now);
        assert_eq!(revisions(inbox.drain_ready(now)), [1]);
        assert!(inbox.has_pending());
        assert_eq!(
            revisions(inbox.drain_ready(now + Duration::from_secs(1))),
            [2]
        );
        assert!(!inbox.has_pending());
        for revision in 3..=5 {
            inbox.push_at(event(true, revision), now);
        }
        assert_eq!(
            revisions(inbox.drain_ready(now + Duration::from_millis(30))),
            [5]
        );
        assert!(!inbox.has_pending());
    }

    #[test]
    fn unmap_and_destroy_collapse_jitter_before_the_lifecycle_barrier() {
        let now = Instant::now();
        let mut inbox = ClientPresentationInbox::default();
        inbox.smooth_source(ClientSourceId::new(1), Duration::from_millis(11));
        for revision in 1..=2 {
            inbox.push_at(event(true, revision), now);
        }
        inbox.push_at(event(false, 3), now);
        inbox.push_at(event(true, 4), now);
        let surface = event(true, 1).surface;
        inbox.push_at(
            ClientSurfaceEvent {
                surface,
                kind: ClientSurfaceEventKind::Destroyed,
            },
            now,
        );
        assert_eq!(revisions(inbox.drain_ready(now)), [2, 3, 4, 0]);
        assert!(!inbox.has_pending());
        assert_eq!(
            inbox.push_at(event(true, 5), now),
            PresentationDemand::Structure
        );
    }

    #[test]
    fn changed_layout_invalidates_history_and_retained_buffers_survive_eviction() {
        use crate::{
            ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, Extent,
            SurfaceBufferChange, SurfaceBufferUpdate, SurfaceLayerId,
        };
        use std::{cell::Cell, rc::Rc};
        let completed = Rc::new(Cell::new(0));
        let mut first = event(true, 1);
        let source = first.surface.source();
        let metadata = ClientBufferMetadata::new(Extent::new(10, 10), true);
        let released = completed.clone();
        let buffer = ClientBufferLease::new(
            ClientBufferId::new(source, 1),
            ClientBufferUseId::new(source, 1),
            metadata,
            Rc::new(()),
            move |_| released.set(released.get() + 1),
        )
        .expect("buffer");
        if let ClientSurfaceEventKind::Commit(commit) = &mut first.kind {
            commit.make_mut().buffers.push(SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(1),
                change: SurfaceBufferChange::Replaced { metadata, buffer },
            });
        }
        let mut inbox = ClientPresentationInbox::default();
        inbox.smooth_source(source, Duration::from_millis(11));
        let now = Instant::now();
        inbox.push_at(first, now);
        for revision in 2..=4 {
            let mut next = event(true, revision);
            if let ClientSurfaceEventKind::Commit(commit) = &mut next.kind {
                commit.make_mut().buffers.push(SurfaceBufferUpdate {
                    layer: SurfaceLayerId::new(1),
                    change: SurfaceBufferChange::Retained { metadata },
                });
            }
            inbox.push_at(next, now);
        }
        assert_eq!(completed.get(), 0);
        let frames = inbox.drain_ready(now).collect::<Vec<_>>();
        assert_eq!(revisions(frames.iter().cloned()), [3]);
        drop(frames);
        assert_eq!(
            completed.get(),
            0,
            "final queued snapshot owns the retained buffer"
        );
        // Removing a layer is a layout barrier; no old pixels can reappear.
        inbox.push_at(event(true, 5), now);
        assert_eq!(completed.get(), 1);
        assert_eq!(revisions(inbox.drain_ready(now)), [5]);
        assert!(!inbox.has_pending());
    }

    #[test]
    fn local_interleaving_matches_the_existing_coalescer() {
        use crate::PendingClientEvents;
        let mut inbox = ClientPresentationInbox::default();
        let mut old = PendingClientEvents::default();
        for (surface, revision, mapped) in [
            (1, 1, true),
            (1, 2, false),
            (2, 1, true),
            (1, 3, false),
            (1, 4, true),
            (2, 2, true),
            (1, 5, true),
        ] {
            let mut event = event(mapped, revision);
            event.surface = ClientSurfaceId::for_test(surface);
            old.push((), event.clone());
            inbox.push(event);
        }
        assert_eq!(
            revisions(inbox.drain()),
            revisions(old.drain().map(|(_, event)| event))
        );
    }

    #[test]
    fn content_coalesces_but_mapping_transitions_keep_order_and_request_layout() {
        let mut inbox = ClientPresentationInbox::default();
        assert_eq!(inbox.push(event(true, 1)), PresentationDemand::Structure);
        assert_eq!(inbox.push(event(true, 2)), PresentationDemand::Content);
        assert_eq!(inbox.push(event(false, 3)), PresentationDemand::Structure);
        assert_eq!(inbox.push(event(true, 4)), PresentationDemand::Structure);
        let commits = inbox
            .drain()
            .map(|event| match event.kind {
                ClientSurfaceEventKind::Commit(commit) => (commit.mapped, commit.revision),
                _ => panic!("expected commit"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            commits,
            [
                (true, ClientCommitRevision::new(2)),
                (false, ClientCommitRevision::new(3)),
                (true, ClientCommitRevision::new(4))
            ]
        );
        assert!(inbox.pop_front().is_none());
    }
}
