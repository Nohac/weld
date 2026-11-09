//! Native tiling configuration and operations, independent of compositor state.

/// Orientation of a split. Horizontal places children left to right.
// Improvement: expose Left/Right/Up/Down split requests in the native API, with
// direction selecting the insertion side. Translate Sway/i3 axis commands in
// the compatibility adapter. See docs/master-tiling.md, "Next improvements".
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SplitAxis {
    #[default]
    Horizontal,
    Vertical,
}

/// Placement of a container's children. Overlapping layouts show one branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TileLayout {
    Split(SplitAxis),
    Tabbed,
    Stacked,
}

impl TileLayout {
    /// Axis used for directional navigation between children.
    pub const fn axis(self) -> SplitAxis {
        match self {
            Self::Split(axis) => axis,
            Self::Tabbed => SplitAxis::Horizontal,
            Self::Stacked => SplitAxis::Vertical,
        }
    }
    pub const fn is_split(self) -> bool {
        matches!(self, Self::Split(_))
    }
}

/// Direction in output-local logical coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Live tiling preferences. Replacing this resource preserves the current tree.
/// Gaps affect existing geometry; default axis affects only newly made splits.
#[cfg_attr(feature = "ecs", derive(bevy_ecs::resource::Resource))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TileSettings {
    /// Logical pixels between siblings, clamped to the available space.
    pub inner_gap: u16,
    /// Logical pixels between the workspace and output edges.
    pub outer_gap: u16,
    /// Remove workspace-edge gaps while one outer tiled frame occupies the workspace.
    pub hide_solo_gaps: bool,
    /// Initial orientation for new workspace roots.
    pub default_axis: SplitAxis,
}

impl Default for TileSettings {
    fn default() -> Self {
        Self {
            inner_gap: 8,
            outer_gap: 8,
            hide_solo_gaps: false,
            default_axis: SplitAxis::Horizontal,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TileOperation {
    /// Requests client close, or removes an empty persistent slot.
    Close,
    /// Wrap the selected node for the next admission, or orient its sole-child
    /// parent. Selecting the workspace root changes its orientation directly.
    Split(SplitAxis),
    Focus(Direction),
    /// Swap leaf positions with the directional neighbor, including across splits.
    Move(Direction),
    /// Adjust this branch against a sibling by a fraction of their combined share.
    Resize {
        axis: SplitAxis,
        fraction: f32,
    },
    /// Adjust this branch against a sibling by logical pixels.
    ResizePixels {
        axis: SplitAxis,
        pixels: f32,
    },
}

mod navigation;
pub use navigation::{
    FocusHierarchy, FocusPlaneRequest, FocusWrapping, LayoutChoice, LayoutRequest, ResizeRequest,
};
