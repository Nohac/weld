//! i3 layout-command targeting and remembered split orientation.

use bevy::ecs::{
    event::Event,
    observer::On,
    system::{Commands, Res, ResMut},
};
use weld_tile::{SplitAxis, TileCommands, TileLayout, TileSelection, TileSetLayout, TileTreeEdit};
use weld_window::{FocusedWindow, workspace::FocusedWorkspace};

use crate::tree::TreeView;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutChoice {
    Layout(TileLayout),
    /// Restore the last split, or alternate the current split orientation.
    Split,
}

/// Layout vocabulary and cycling belong to the i3 command interpreter.
#[derive(Event, Clone, Debug, PartialEq)]
pub enum I3LayoutRequest {
    Set(TileLayout),
    Default,
    Toggle,
    ToggleAll,
    Cycle(Vec<LayoutChoice>),
}

fn opposite(axis: SplitAxis) -> SplitAxis {
    match axis {
        SplitAxis::Horizontal => SplitAxis::Vertical,
        SplitAxis::Vertical => SplitAxis::Horizontal,
    }
}

impl I3LayoutRequest {
    fn resolve(&self, current: TileLayout, last_split: SplitAxis) -> TileLayout {
        match self {
            Self::Set(layout) => *layout,
            Self::Default => TileLayout::Split(last_split),
            Self::Toggle => match current {
                TileLayout::Split(_) => TileLayout::Stacked,
                TileLayout::Stacked => TileLayout::Tabbed,
                TileLayout::Tabbed => TileLayout::Split(last_split),
            },
            Self::ToggleAll => match current {
                TileLayout::Split(SplitAxis::Horizontal) => TileLayout::Split(SplitAxis::Vertical),
                TileLayout::Split(SplitAxis::Vertical) => TileLayout::Stacked,
                TileLayout::Stacked => TileLayout::Tabbed,
                TileLayout::Tabbed => TileLayout::Split(SplitAxis::Horizontal),
            },
            Self::Cycle(choices) => {
                let index = choices.iter().position(|choice| match choice {
                    LayoutChoice::Layout(layout) => *layout == current,
                    LayoutChoice::Split => current.is_split(),
                });
                let next = index
                    .and_then(|index| choices.get(index + 1))
                    .or_else(|| choices.first());
                match next {
                    Some(LayoutChoice::Layout(layout)) => *layout,
                    Some(LayoutChoice::Split) => TileLayout::Split(if current.is_split() {
                        opposite(last_split)
                    } else {
                        last_split
                    }),
                    None => current,
                }
            }
        }
    }
}

pub(crate) fn request(
    event: On<I3LayoutRequest>,
    tree: TreeView,
    focus: Res<FocusedWindow>,
    selection: Res<TileSelection>,
    workspace: Res<FocusedWorkspace>,
    mut pending: ResMut<TileCommands>,
    mut commands: Commands,
) {
    if tree.workspaces.is_empty() {
        let _ = pending.defer(event.event().clone());
        return;
    }
    let Some(node) = selection.target(&focus).or(workspace.entity()) else {
        return;
    };
    let container = if tree.workspaces.contains(node) {
        node
    } else if let Ok(parent) = tree.parents.get(node) {
        parent.entity()
    } else {
        return;
    };
    let Ok(state) = tree.containers.get(container) else {
        return;
    };
    let layout = event.resolve(state.layout(), state.last_split_axis());
    if tree.workspaces.contains(container)
        && state.children().next().is_some()
        && !matches!(event.event(), I3LayoutRequest::Default)
    {
        commands.trigger(TileTreeEdit::GroupChildren { container, layout });
    } else {
        // Avoid an ever-growing unary chain when alternating split preparation
        // with a tabbed/stacked layout (i3 issue #3001).
        let container = if let Ok(parent) = tree.parents.get(container)
            && !tree.workspaces.contains(parent.entity())
            && state.children().count() == 1
            && tree
                .containers
                .get(parent.entity())
                .is_ok_and(|outer| outer.children().count() == 1)
        {
            commands.trigger(TileTreeEdit::Collapse { container });
            parent.entity()
        } else {
            container
        };
        commands.trigger(TileSetLayout { container, layout });
    }
}
