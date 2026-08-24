//! Retained client-surface roles, commits, and bidirectional policy requests.

use std::collections::{HashMap, VecDeque};

use crate::{
    ClientBufferLease, ClientBufferMetadata, ClientOutputId, ClientSourceId, ClientSurfaceId,
    Extent, LogicalPoint, LogicalSize, SurfaceLayerId,
};

/// Monotonic commit observation used by configure-settlement policy.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClientCommitRevision(u64);

impl ClientCommitRevision {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Which side owns a toplevel's visible frame and titlebar.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WindowDecoration {
    #[default]
    ClientSide,
    ServerSide,
}

/// Current role state for an independently managed toplevel.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToplevelState {
    pub parent: Option<ClientSurfaceId>,
    pub decoration: WindowDecoration,
}

/// Current protocol-owned popup placement relative to its owning toplevel.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PopupState {
    pub owner: ClientSurfaceId,
    pub position: LogicalPoint,
    pub stack_index: i32,
}

/// Complete current role of one independently identified client surface.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClientSurfaceRole {
    Toplevel(ToplevelState),
    Popup(PopupState),
}

/// The displayed part of a client buffer and its logical extent.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceContentView {
    pub source_x: f32,
    pub source_y: f32,
    pub source_width: f32,
    pub source_height: f32,
    pub logical_width: f32,
    pub logical_height: f32,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceLayerPlacement {
    pub layer: SurfaceLayerId,
    pub position: LogicalPoint,
    pub view: SurfaceContentView,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceWindowGeometry {
    pub origin: LogicalPoint,
    pub view: SurfaceContentView,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceInputRect {
    pub position: LogicalPoint,
    pub size: LogicalSize,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceInputPlacement {
    pub layer: SurfaceLayerId,
    pub position: LogicalPoint,
    pub regions: Vec<SurfaceInputRect>,
}

/// Content transition for one layer in a complete retained tree commit.
#[derive(Clone, Debug)]
pub enum SurfaceBufferChange {
    Retained {
        metadata: ClientBufferMetadata,
    },
    Replaced {
        metadata: ClientBufferMetadata,
        buffer: ClientBufferLease,
    },
    Removed,
}

impl SurfaceBufferChange {
    pub const fn metadata(&self) -> Option<ClientBufferMetadata> {
        match self {
            Self::Retained { metadata } | Self::Replaced { metadata, .. } => Some(*metadata),
            Self::Removed => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SurfaceBufferUpdate {
    pub layer: SurfaceLayerId,
    pub change: SurfaceBufferChange,
}

/// Complete current surface-tree geometry plus changed buffer uses.
/// One atomic surface commit.
///
/// Cloning a commit retains every embedded [`ClientBufferLease`] and therefore
/// extends those committed buffer uses until all clones are dropped.
#[derive(Clone, Debug)]
pub struct ClientSurfaceCommit {
    pub revision: ClientCommitRevision,
    pub mapped: bool,
    pub root: Option<SurfaceLayerPlacement>,
    pub window_geometry: Option<SurfaceWindowGeometry>,
    pub overlays: Vec<SurfaceLayerPlacement>,
    pub inputs: Vec<SurfaceInputPlacement>,
    pub buffers: Vec<SurfaceBufferUpdate>,
}

impl ClientSurfaceCommit {
    pub fn carry_unobserved_content_from(&mut self, previous: &mut Self) {
        let mut pending = previous
            .buffers
            .iter_mut()
            .filter_map(|buffer| {
                let change = std::mem::replace(&mut buffer.change, SurfaceBufferChange::Removed);
                matches!(change, SurfaceBufferChange::Replaced { .. })
                    .then_some((buffer.layer, change))
            })
            .collect::<HashMap<_, _>>();
        for buffer in &mut self.buffers {
            if matches!(buffer.change, SurfaceBufferChange::Retained { .. })
                && let Some(change) = pending.remove(&buffer.layer)
            {
                buffer.change = change;
            }
        }
    }
}

/// Client-to-compositor request for an interactive move or resize.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToplevelInteractionRequestKind {
    Move,
    Resize { edges: WindowResizeEdge },
    End,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowResizeEdge {
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    BottomLeft,
    TopRight,
    BottomRight,
}

impl WindowResizeEdge {
    pub const fn has_left(self) -> bool {
        matches!(self, Self::Left | Self::TopLeft | Self::BottomLeft)
    }

    pub const fn has_right(self) -> bool {
        matches!(self, Self::Right | Self::TopRight | Self::BottomRight)
    }

    pub const fn has_top(self) -> bool {
        matches!(self, Self::Top | Self::TopLeft | Self::TopRight)
    }

    pub const fn has_bottom(self) -> bool {
        matches!(self, Self::Bottom | Self::BottomLeft | Self::BottomRight)
    }
}

#[derive(Clone, Debug)]
pub struct ClientSurfaceEvent {
    pub surface: ClientSurfaceId,
    pub kind: ClientSurfaceEventKind,
}

#[derive(Clone, Debug)]
pub enum ClientSurfaceEventKind {
    Role(ClientSurfaceRole),
    Commit(ClientSurfaceCommit),
    Interaction(ToplevelInteractionRequestKind),
    Destroyed,
}

/// Adjacent event queue that preserves the newest unobserved buffer use.
#[derive(Default)]
pub struct ClientEventQueue(VecDeque<ClientSurfaceEvent>);

impl ClientEventQueue {
    pub fn push(&mut self, event: ClientSurfaceEvent) {
        let ClientSurfaceEvent { surface, mut kind } = event;
        if let ClientSurfaceEventKind::Commit(current) = &mut kind
            && let Some(ClientSurfaceEvent {
                surface: previous_surface,
                kind: ClientSurfaceEventKind::Commit(previous),
            }) = self.0.back_mut()
            && *previous_surface == surface
            && previous.mapped == current.mapped
        {
            current.carry_unobserved_content_from(previous);
            self.0.pop_back();
        }
        self.0.push_back(ClientSurfaceEvent { surface, kind });
    }

    pub fn pop_front(&mut self) -> Option<ClientSurfaceEvent> {
        self.0.pop_front()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// One surface-addressed compositor-to-client policy request.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientSurfaceRequest {
    pub surface: ClientSurfaceId,
    pub kind: ClientSurfaceRequestKind,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientSurfaceRequestKind {
    Close,
    Configure {
        logical_size: Extent,
    },
    SetOutputs {
        outputs: Vec<ClientOutputId>,
        preferred: Option<ClientOutputId>,
        preferred_scale_120: Option<u32>,
    },
    SetPreferredScale {
        scale_120: Option<u32>,
    },
}

/// Source-addressed keyboard focus request; `None` clears that source's focus.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientFocusRequest {
    pub source: ClientSourceId,
    pub surface: Option<ClientSurfaceId>,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientRequest {
    Surface(ClientSurfaceRequest),
    Focus(ClientFocusRequest),
    /// Clears the runtime's currently routed keyboard focus, regardless of source.
    ClearFocus,
}

impl ClientRequest {
    pub const fn source(&self) -> Option<ClientSourceId> {
        match self {
            Self::Surface(request) => Some(request.surface.source()),
            Self::Focus(request) => Some(request.source),
            Self::ClearFocus => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{ClientBufferId, ClientBufferUseId, ClientId};

    fn surface() -> ClientSurfaceId {
        ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 2), 3)
    }

    fn commit(revision: u64, change: SurfaceBufferChange) -> ClientSurfaceEvent {
        ClientSurfaceEvent {
            surface: surface(),
            kind: ClientSurfaceEventKind::Commit(ClientSurfaceCommit {
                revision: ClientCommitRevision::new(revision),
                mapped: true,
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: vec![SurfaceBufferUpdate {
                    layer: SurfaceLayerId::new(1),
                    change,
                }],
            }),
        }
    }

    fn lease(local: u64, completed: Rc<Cell<u32>>) -> ClientBufferLease {
        let source = ClientSourceId::new(1);
        ClientBufferLease::new(
            ClientBufferId::new(source, local),
            ClientBufferUseId::new(source, local),
            ClientBufferMetadata::new(Extent::new(1, 1), false),
            Rc::new(()),
            move |_| completed.set(completed.get() + 1),
        )
        .expect("matching source identity")
    }

    #[test]
    fn adjacent_commit_carries_the_newest_unobserved_replacement() {
        let completed = Rc::new(Cell::new(0));
        let expected = lease(7, completed.clone());
        let mut queue = ClientEventQueue::default();
        queue.push(commit(
            1,
            SurfaceBufferChange::Replaced {
                metadata: expected.metadata(),
                buffer: expected.clone(),
            },
        ));
        queue.push(commit(
            2,
            SurfaceBufferChange::Retained {
                metadata: expected.metadata(),
            },
        ));

        let event = queue.pop_front().expect("coalesced commit");
        let ClientSurfaceEventKind::Commit(commit) = &event.kind else {
            panic!("expected commit");
        };
        let SurfaceBufferChange::Replaced { buffer, .. } = &commit.buffers[0].change else {
            panic!("expected carried replacement");
        };
        assert!(buffer.same_use(&expected));
        drop(event);
        drop(expected);
        assert_eq!(completed.get(), 1);
    }

    #[test]
    fn a_new_replacement_completes_the_superseded_unobserved_use() {
        let first_completed = Rc::new(Cell::new(0));
        let second_completed = Rc::new(Cell::new(0));
        let first = lease(1, first_completed.clone());
        let second = lease(2, second_completed.clone());
        let mut queue = ClientEventQueue::default();
        queue.push(commit(
            1,
            SurfaceBufferChange::Replaced {
                metadata: first.metadata(),
                buffer: first,
            },
        ));
        queue.push(commit(
            2,
            SurfaceBufferChange::Replaced {
                metadata: second.metadata(),
                buffer: second,
            },
        ));

        assert_eq!(first_completed.get(), 1);
        assert_eq!(second_completed.get(), 0);
        drop(queue);
        assert_eq!(second_completed.get(), 1);
    }

    #[test]
    fn role_map_unmap_and_destroy_keep_their_lifecycle_order() {
        let surface = surface();
        let mut queue = ClientEventQueue::default();
        queue.push(ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                parent: None,
                decoration: WindowDecoration::ServerSide,
            })),
        });
        queue.push(commit(
            1,
            SurfaceBufferChange::Retained {
                metadata: ClientBufferMetadata::new(Extent::new(1, 1), true),
            },
        ));
        let mut unmap = commit(2, SurfaceBufferChange::Removed);
        let ClientSurfaceEventKind::Commit(unmap_commit) = &mut unmap.kind else {
            panic!("expected commit");
        };
        unmap_commit.mapped = false;
        queue.push(unmap);
        queue.push(ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Destroyed,
        });

        assert!(matches!(
            queue.pop_front().map(|event| event.kind),
            Some(ClientSurfaceEventKind::Role(_))
        ));
        assert!(matches!(
            queue.pop_front().map(|event| event.kind),
            Some(ClientSurfaceEventKind::Commit(commit)) if commit.mapped
        ));
        assert!(matches!(
            queue.pop_front().map(|event| event.kind),
            Some(ClientSurfaceEventKind::Commit(commit)) if !commit.mapped
        ));
        assert!(matches!(
            queue.pop_front().map(|event| event.kind),
            Some(ClientSurfaceEventKind::Destroyed)
        ));
        assert!(queue.is_empty());
    }
}
