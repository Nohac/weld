//! Retained client-surface roles, commits, and bidirectional policy requests.

use std::{ops::Deref, rc::Rc};

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
    pub hints: ToplevelHints,
}

/// Client-provided facts used for initial placement and floating size limits.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ToplevelHints {
    pub kind: ToplevelKind,
    /// Zero means no client minimum on that axis.
    pub min_size: Extent,
    /// Zero means no client maximum on that axis.
    pub max_size: Extent,
}

impl ToplevelHints {
    pub fn prefers_floating(self) -> bool {
        self.kind != ToplevelKind::Normal
            || (self.min_size.width > 0
                && self.min_size.height > 0
                && (self.min_size.width == self.max_size.width
                    || self.min_size.height == self.max_size.height))
    }

    pub fn constrain(self, size: Extent) -> Extent {
        fn dimension(value: u32, minimum: u32, maximum: u32) -> u32 {
            let minimum = minimum.max(1);
            let maximum = if maximum == 0 {
                u32::MAX
            } else {
                maximum.max(minimum)
            };
            value.clamp(minimum, maximum)
        }
        Extent::new(
            dimension(size.width, self.min_size.width, self.max_size.width),
            dimension(size.height, self.min_size.height, self.max_size.height),
        )
    }
}

#[cfg(test)]
mod hint_tests {
    use super::*;
    #[test]
    fn size_limits_handle_zero_and_inconsistent_maxima() {
        let limits = ToplevelHints {
            min_size: Extent::new(400, 300),
            max_size: Extent::new(200, 0),
            ..Default::default()
        };
        assert_eq!(
            limits.constrain(Extent::new(100, 200)),
            Extent::new(400, 300)
        );
        assert_eq!(
            limits.constrain(Extent::new(1000, 900)),
            Extent::new(400, 900)
        );
        assert_eq!(
            ToplevelHints::default().constrain(Extent::new(0, 0)),
            Extent::new(1, 1)
        );
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToplevelKind {
    #[default]
    Normal,
    Dialog,
    Utility,
    Toolbar,
    Splash,
}

/// Presenter-selected placement mode, applied atomically with size/state.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToplevelLayout {
    #[default]
    Floating,
    Tiled,
}

/// Current protocol-owned popup placement relative to its owning root surface.
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
    /// Output-bound desktop content, excluded from independent window hoisting.
    Layer(LayerSurfaceState),
}

/// Output-local desktop surface arranged by the host's layer-shell implementation.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerSurfaceState {
    pub output: ClientOutputId,
    pub position: LogicalPoint,
    pub layer: DesktopLayer,
    pub keyboard: LayerKeyboardInteractivity,
    pub stack_index: u32,
}

/// Desktop composition order, from wallpaper to interactive overlays.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DesktopLayer {
    Background,
    Bottom,
    Top,
    Overlay,
}

/// Keyboard focus requested by a desktop surface.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayerKeyboardInteractivity {
    None,
    Exclusive,
    OnDemand,
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

/// Whether an adapter has discarded the original surface tree's transparency.
///
/// This is independent of an individual buffer's opaque pixel format. An
/// opaque transport cannot reproduce visual overflow such as client shadows,
/// even though the application still declares the same window geometry and
/// decoration preference.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SurfaceAlphaMode {
    /// Original alpha semantics are intact, including naturally opaque buffers.
    #[default]
    Preserved,
    /// Alpha has been lost through an opaque media path.
    Discarded,
}

/// Complete current surface-tree geometry plus changed buffer uses.
/// One atomic surface commit.
///
/// Consumers share the immutable snapshot and its buffer leases. Mutation is
/// explicit through [`Self::make_mut`]; rendering/encoding may take ownership
/// through [`Self::into_state`] once that consumer is ready.
#[derive(Clone, Debug)]
pub struct ClientSurfaceCommit(Rc<ClientSurfaceState>);

/// One complete surface inventory and the buffer updates applied atomically to it.
#[derive(Clone, Debug)]
pub struct ClientSurfaceState {
    pub revision: ClientCommitRevision,
    pub alpha_mode: SurfaceAlphaMode,
    pub mapped: bool,
    pub root: Option<SurfaceLayerPlacement>,
    pub window_geometry: Option<SurfaceWindowGeometry>,
    pub overlays: Vec<SurfaceLayerPlacement>,
    pub inputs: Vec<SurfaceInputPlacement>,
    /// Complete current buffer inventory, not an incremental update list.
    /// Unchanged layers must appear as [`SurfaceBufferChange::Retained`].
    /// Omission or [`SurfaceBufferChange::Removed`] retires a layer's buffer;
    /// geometry visibility alone does not change this inventory.
    pub buffers: Vec<SurfaceBufferUpdate>,
}

impl ClientSurfaceCommit {
    /// Copy on write keeps other consumers' snapshots unchanged.
    pub fn make_mut(&mut self) -> &mut ClientSurfaceState {
        Rc::make_mut(&mut self.0)
    }

    /// Reuse an exclusively owned snapshot, or copy it for this consumer.
    pub fn into_state(self) -> ClientSurfaceState {
        Rc::unwrap_or_clone(self.0)
    }

    /// Carry buffers not yet consumed by this reader through retained updates.
    /// Fully replaced inventories need no allocation or snapshot mutation.
    pub fn carry_unobserved_content_from(&mut self, previous: &Self) {
        for index in 0..self.buffers.len() {
            let buffer = &self.buffers[index];
            if !matches!(buffer.change, SurfaceBufferChange::Retained { .. }) {
                continue;
            }
            let Some(change) = previous.buffers.iter().find_map(|previous| {
                (previous.layer == buffer.layer
                    && matches!(previous.change, SurfaceBufferChange::Replaced { .. }))
                .then_some(&previous.change)
            }) else {
                continue;
            };
            self.make_mut().buffers[index].change = change.clone();
        }
    }
}

impl From<ClientSurfaceState> for ClientSurfaceCommit {
    fn from(state: ClientSurfaceState) -> Self {
        Self(Rc::new(state))
    }
}

impl Deref for ClientSurfaceCommit {
    type Target = ClientSurfaceState;

    fn deref(&self) -> &Self::Target {
        &self.0
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

/// Client intent; the active window manager decides whether to grant it.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToplevelStateRequestKind {
    Fullscreen(bool),
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
    StateRequest(ToplevelStateRequestKind),
    Destroyed,
    Metadata(crate::ClientSurfaceMetadata),
}

/// Runtime event queue retaining the latest unobserved buffer uses across surfaces.
#[derive(Default)]
pub struct ClientEventQueue(crate::PendingClientEvents<()>);

impl ClientEventQueue {
    pub fn push(&mut self, event: ClientSurfaceEvent) {
        self.0.push((), event);
    }

    pub fn pop_front(&mut self) -> Option<ClientSurfaceEvent> {
        self.0.pop_front().map(|(_, event)| event)
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
        layout: ToplevelLayout,
        resizing: bool,
        fullscreen: bool,
    },
    SetOutputs {
        outputs: Vec<ClientOutputId>,
        preferred: Option<ClientOutputId>,
        preferred_scale_120: Option<u32>,
    },
    SetPreferredScale {
        scale_120: Option<u32>,
    },
    /// Presenter cadence; `None` explicitly suspends frame opportunities.
    SetPresentation {
        rate: Option<crate::PresentationRate>,
    },
    /// Encoded quality grouping only; `None` restores ordinary window allocation.
    /// No focus, parentage, scheduling priority or additional authority is granted.
    SetBitratePreference {
        preference: Option<crate::SurfaceBitratePreference>,
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
            kind: ClientSurfaceEventKind::Commit(ClientSurfaceCommit::from(
                crate::ClientSurfaceState {
                    revision: ClientCommitRevision::new(revision),
                    alpha_mode: Default::default(),
                    mapped: true,
                    root: None,
                    window_geometry: None,
                    overlays: Vec::new(),
                    inputs: Vec::new(),
                    buffers: vec![SurfaceBufferUpdate {
                        layer: SurfaceLayerId::new(1),
                        change,
                    }],
                },
            )),
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
    fn snapshots_share_inventory_until_mutation_and_move_unique_storage() {
        let event = commit(1, SurfaceBufferChange::Removed);
        let ClientSurfaceEventKind::Commit(original) = event.kind else {
            panic!("commit fixture");
        };
        let mut changed = original.clone();
        assert!(Rc::ptr_eq(&original.0, &changed.0));
        assert_eq!(original.buffers.as_ptr(), changed.buffers.as_ptr());
        changed.make_mut().revision = ClientCommitRevision::new(2);
        assert_eq!(original.revision.raw(), 1);
        assert!(!Rc::ptr_eq(&original.0, &changed.0));
        let unique_storage = changed.buffers.as_ptr();
        let owned = changed.into_state();
        assert_eq!(unique_storage, owned.buffers.as_ptr());
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
                hints: Default::default(),
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
        unmap_commit.make_mut().mapped = false;
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
