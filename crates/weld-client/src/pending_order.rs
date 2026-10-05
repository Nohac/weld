//! Ordered coalescing decisions shared by transport and presentation consumers.

use crate::{ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceId};

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Commit { mapped: bool },
    Metadata,
    Control,
}
impl Kind {
    pub fn of(event: &ClientSurfaceEvent) -> Self {
        match &event.kind {
            ClientSurfaceEventKind::Commit(commit) => Self::Commit {
                mapped: commit.mapped,
            },
            ClientSurfaceEventKind::Metadata(_) => Self::Metadata,
            _ => Self::Control,
        }
    }
}

pub(super) struct Record {
    pub surface: ClientSurfaceId,
    pub kind: Kind,
    pub same_scope: bool,
    pub barrier: bool,
}

#[derive(Default)]
pub(super) struct Insertion {
    pub replacement: Option<usize>,
    pub barrier: bool,
}

pub(super) fn plan(
    surface: ClientSurfaceId,
    kind: Kind,
    records: impl DoubleEndedIterator<Item = (usize, Record)>,
) -> Insertion {
    match kind {
        Kind::Commit { mapped } => {
            for (index, record) in records.rev() {
                match record.kind {
                    Kind::Metadata => continue,
                    Kind::Commit { mapped: previous } => {
                        if record.surface == surface {
                            let compatible = record.same_scope && previous == mapped;
                            return Insertion {
                                replacement: compatible.then_some(index),
                                barrier: !compatible || record.barrier,
                            };
                        }
                        if record.barrier {
                            break;
                        }
                    }
                    Kind::Control => break,
                }
            }
            Insertion {
                replacement: None,
                barrier: !mapped,
            }
        }
        Kind::Metadata => {
            for (index, record) in records.rev() {
                if record.same_scope
                    && record.surface == surface
                    && matches!(record.kind, Kind::Metadata)
                {
                    return Insertion {
                        replacement: Some(index),
                        barrier: false,
                    };
                }
                if record.barrier || matches!(record.kind, Kind::Control) {
                    break;
                }
            }
            Insertion::default()
        }
        Kind::Control => Insertion::default(),
    }
}
