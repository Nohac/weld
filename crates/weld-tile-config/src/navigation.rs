//! Selection, wrapping, layout cycles and resize intent over a split tree.

use crate::{SplitAxis, TileLayout};

/// Select an ancestor split or its last-selected direct child.
#[cfg_attr(feature = "ecs", derive(bevy_ecs::event::Event))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FocusHierarchy {
    Parent,
    Child,
}

/// Select the most recently focused member of a layout plane in the active workspace.
#[cfg_attr(feature = "ecs", derive(bevy_ecs::event::Event))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FocusPlaneRequest {
    Tiled,
    Floating,
    Toggle,
}

/// Where directional navigation may wrap after reaching a split edge.
#[cfg_attr(feature = "ecs", derive(bevy_ecs::resource::Resource))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FocusWrapping {
    Disabled,
    #[default]
    Fallback,
    Immediate,
    /// Keeps navigation within the current workspace. Directional output
    /// traversal remains a follow-up, so currently this matches [`Self::Fallback`].
    WithinWorkspace,
}

/// A layout candidate evaluated against the active and remembered split layouts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutChoice {
    Layout(TileLayout),
    /// Match either split orientation and restore the remembered orientation.
    RememberedSplit,
    /// Match either split orientation; alternate when currently split, otherwise restore.
    AlternateSplit,
}

/// Change layout or cycle an explicitly ordered set of native choices.
#[cfg_attr(feature = "ecs", derive(bevy_ecs::event::Event))]
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutRequest {
    Set(TileLayout),
    RestoreSplit,
    Cycle(Vec<LayoutChoice>),
}

impl LayoutRequest {
    /// Resolve live layout history when the operation executes.
    pub fn resolve(&self, current: TileLayout, last_split: SplitAxis) -> TileLayout {
        match self {
            Self::Set(layout) => *layout,
            Self::RestoreSplit => TileLayout::Split(last_split),
            Self::Cycle(choices) => {
                let index = choices.iter().position(|choice| match choice {
                    LayoutChoice::Layout(layout) => *layout == current,
                    LayoutChoice::RememberedSplit | LayoutChoice::AlternateSplit => {
                        current.is_split()
                    }
                });
                let next = index
                    .and_then(|index| choices.get(index + 1))
                    .or_else(|| choices.first());
                match next {
                    Some(LayoutChoice::Layout(layout)) => *layout,
                    Some(LayoutChoice::RememberedSplit) => TileLayout::Split(last_split),
                    Some(LayoutChoice::AlternateSplit) => {
                        TileLayout::Split(if current.is_split() {
                            match last_split {
                                SplitAxis::Horizontal => SplitAxis::Vertical,
                                SplitAxis::Vertical => SplitAxis::Horizontal,
                            }
                        } else {
                            last_split
                        })
                    }
                    None => current,
                }
            }
        }
    }
}
#[cfg_attr(feature = "ecs", derive(bevy_ecs::event::Event))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResizeRequest {
    pub axis: SplitAxis,
    pub pixels: Option<f32>,
    pub fraction: Option<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_order_and_split_history_are_explicit_native_intent() {
        let cycle = LayoutRequest::Cycle(vec![
            LayoutChoice::Layout(TileLayout::Stacked),
            LayoutChoice::Layout(TileLayout::Tabbed),
            LayoutChoice::RememberedSplit,
        ]);
        let split = TileLayout::Split(SplitAxis::Vertical);
        assert_eq!(
            cycle.resolve(split, SplitAxis::Vertical),
            TileLayout::Stacked
        );
        assert_eq!(
            cycle.resolve(TileLayout::Stacked, SplitAxis::Vertical),
            TileLayout::Tabbed
        );
        assert_eq!(
            cycle.resolve(TileLayout::Tabbed, SplitAxis::Vertical),
            split
        );
        let alternate = LayoutRequest::Cycle(vec![LayoutChoice::AlternateSplit]);
        assert_eq!(
            alternate.resolve(split, SplitAxis::Vertical),
            TileLayout::Split(SplitAxis::Horizontal)
        );
        assert_eq!(
            alternate.resolve(TileLayout::Tabbed, SplitAxis::Vertical),
            split
        );
    }
}
