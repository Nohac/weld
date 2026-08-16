//! Protocol-neutral surface identities and host/application contracts.

/// Stable compositor identity for one client surface.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SurfaceId(u64);

impl SurfaceId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Stable identity for one buffer-bearing surface-tree layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SurfaceLayerId(u64);

impl SurfaceLayerId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// A point in compositor-logical coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LogicalPoint {
    pub x: f32,
    pub y: f32,
}

impl LogicalPoint {
    pub const ZERO: Self = Self::new(0.0, 0.0);

    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

impl From<(f32, f32)> for LogicalPoint {
    fn from((x, y): (f32, f32)) -> Self {
        Self::new(x, y)
    }
}

/// A size in compositor-logical coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LogicalSize {
    pub width: f32,
    pub height: f32,
}

impl LogicalSize {
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

/// An unsigned physical or logical extent.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Extent {
    pub width: u32,
    pub height: u32,
}

impl Extent {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// Which side owns the visible frame and titlebar.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WindowDecoration {
    #[default]
    ClientSide,
    ServerSide,
}

/// Protocol-owned popup placement relative to its owning window geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PopupDescriptor {
    pub owner: SurfaceId,
    pub position: LogicalPoint,
    pub stack_index: i32,
}

/// Edge or corner selected by a client for an interactive resize.
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowInteractionRequestKind {
    Move,
    Resize { edges: WindowResizeEdge },
    End,
}

/// Protocol-neutral request emitted by application policy for the host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceAction {
    Close {
        surface: SurfaceId,
    },
    Focus {
        surface: Option<SurfaceId>,
    },
    Resize {
        surface: SurfaceId,
        logical_size: Extent,
    },
    SetOutputs {
        surface: SurfaceId,
        outputs: Vec<crate::OutputId>,
        preferred: Option<crate::OutputId>,
    },
}

/// The displayed part of a client buffer and its logical extent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceContentView {
    pub source_x: f32,
    pub source_y: f32,
    pub source_width: f32,
    pub source_height: f32,
    pub logical_width: f32,
    pub logical_height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceLayerPlacement {
    pub layer: SurfaceLayerId,
    pub position: LogicalPoint,
    pub view: SurfaceContentView,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceWindowGeometry {
    pub origin: LogicalPoint,
    pub view: SurfaceContentView,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceInputRect {
    pub position: LogicalPoint,
    pub size: LogicalSize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceInputPlacement {
    pub layer: SurfaceLayerId,
    pub position: LogicalPoint,
    pub regions: Vec<SurfaceInputRect>,
}
