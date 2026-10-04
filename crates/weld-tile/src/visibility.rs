//! Active-branch selection shared by geometry and local presentation demand.

use bevy::ecs::{entity::Entity, system::Query};

use crate::{TileContainer, TileFocusHistory, TileParent};

pub(crate) fn branch_visible(
    mut node: Entity,
    parents: &Query<&TileParent>,
    containers: &Query<&TileContainer>,
    history: &TileFocusHistory,
) -> bool {
    for _ in 0..=crate::MAX_DEPTH {
        let Ok(parent) = parents.get(node) else {
            return true;
        };
        let Ok(container) = containers.get(parent.entity()) else {
            return false;
        };
        if !container.layout().is_split() && history.active_child(container) != Some(node) {
            return false;
        }
        node = parent.entity();
    }
    false
}
