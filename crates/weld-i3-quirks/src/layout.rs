//! i3 layout-command targeting and remembered split orientation.

use bevy::ecs::{
    observer::On,
    system::{Commands, Res, ResMut},
};
use weld_tile::{TileCommands, TileSelection, TileSetLayout, TileTreeEdit};
use weld_tile_config::LayoutRequest;
use weld_window::{FocusedWindow, workspace::FocusedWorkspace};

use crate::tree::TreeView;

pub(crate) fn request(
    event: On<LayoutRequest>,
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
        && !matches!(event.event(), LayoutRequest::RestoreSplit)
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
