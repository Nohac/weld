//! Borrowed queries over the shared tiling tree for i3 behavior decisions.

use bevy::ecs::{
    entity::Entity,
    system::{Query, Res, SystemParam},
};
use weld_tile::{TileContainer, TileFocusHistory, TileParent, TileWorkspace};
use weld_window::{ManagedBy, ManagedWindow};

#[derive(SystemParam)]
pub(crate) struct TreeView<'w, 's> {
    pub(crate) containers: Query<'w, 's, &'static TileContainer>,
    pub(crate) parents: Query<'w, 's, &'static TileParent>,
    pub(crate) workspaces: Query<'w, 's, &'static TileWorkspace>,
    pub(crate) windows: Query<'w, 's, (&'static ManagedWindow, &'static ManagedBy)>,
    pub(crate) history: Res<'w, TileFocusHistory>,
}

impl TreeView<'_, '_> {
    pub(crate) fn belongs_to(&self, window: Entity, ancestor: Entity) -> bool {
        let Ok((_, owner)) = self.windows.get(window) else {
            return false;
        };
        let mut node = window;
        let mut found = node == ancestor;
        while let Ok(parent) = self.parents.get(node) {
            node = parent.entity();
            found |= node == ancestor;
        }
        found && owner.0 == node && self.workspaces.contains(node)
    }

    pub(crate) fn descend(&self, node: Entity) -> Option<Entity> {
        if self.belongs_to(node, node) {
            return Some(node);
        }
        self.history
            .recent()
            .filter(|child| {
                self.parents
                    .get(*child)
                    .is_ok_and(|parent| parent.entity() == node)
            })
            .find_map(|child| self.descend(child))
            .or_else(|| {
                self.containers
                    .get(node)
                    .ok()?
                    .children()
                    .find_map(|(child, _)| self.descend(child))
            })
    }
}
