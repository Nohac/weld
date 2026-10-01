//! Tree traversal and branch-local restoration, using shared node history.

use bevy::{
    ecs::{
        entity::Entity,
        message::MessageWriter,
        observer::On,
        query::Changed,
        resource::Resource,
        system::{Commands, Query, Res, ResMut, SystemParam},
    },
    window::RequestRedraw,
};
use weld_tile::{
    Direction, SplitAxis, TileCommands, TileContainer, TileFocusHistory, TileParent, TileWorkspace,
};
use weld_window::{
    FocusedWindow, ManagedBy, ManagedWindow, WindowCommand, WindowCommandKind, WindowFocusChanged,
};

use crate::{FocusWrapping, I3FocusRequest};

/// Preserves ancestry across destruction so recovery can run before compaction.
#[derive(Resource, Default)]
pub(crate) struct FocusPath {
    window: Option<Entity>,
    ancestors: Vec<Entity>,
}

#[derive(SystemParam)]
pub(crate) struct FocusTree<'w, 's> {
    containers: Query<'w, 's, &'static TileContainer>,
    parents: Query<'w, 's, &'static TileParent>,
    workspaces: Query<'w, 's, &'static TileWorkspace>,
    windows: Query<'w, 's, (&'static ManagedWindow, &'static ManagedBy)>,
    history: Res<'w, TileFocusHistory>,
}

impl FocusTree<'_, '_> {
    fn belongs_to(&self, window: Entity, ancestor: Entity) -> bool {
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

    fn descend(&self, node: Entity) -> Option<Entity> {
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

    fn navigate(
        &self,
        window: Entity,
        direction: Direction,
        wrapping: FocusWrapping,
    ) -> Option<Entity> {
        if !self.belongs_to(window, window) {
            return None;
        }
        let axis = match direction {
            Direction::Left | Direction::Right => SplitAxis::Horizontal,
            Direction::Up | Direction::Down => SplitAxis::Vertical,
        };
        let forward = matches!(direction, Direction::Right | Direction::Down);
        let mut branch = window;
        let mut wrap = None;
        while let Ok(parent) = self.parents.get(branch) {
            let parent = parent.entity();
            let container = self.containers.get(parent).ok()?;
            if container.axis() == axis {
                let index = container
                    .children()
                    .position(|(child, _)| child == branch)?;
                let next = if forward {
                    index.checked_add(1)
                } else {
                    index.checked_sub(1)
                };
                if let Some((neighbor, _)) = next.and_then(|index| container.children().nth(index))
                {
                    return self.descend(neighbor);
                }
                if wrapping != FocusWrapping::No
                    && wrap.is_none()
                    && container.children().count() > 1
                {
                    wrap = if forward {
                        container.children().next()
                    } else {
                        container.children().last()
                    }
                    .map(|(child, _)| child);
                    if wrapping == FocusWrapping::Force {
                        return wrap.and_then(|node| self.descend(node));
                    }
                }
            }
            branch = parent;
        }
        wrap.and_then(|node| self.descend(node))
    }

    fn remember(&self, window: Option<Entity>, path: &mut FocusPath) {
        path.window = window.filter(|window| self.belongs_to(*window, *window));
        path.ancestors.clear();
        if let Some(mut node) = path.window {
            while let Ok(parent) = self.parents.get(node) {
                node = parent.entity();
                path.ancestors.push(node);
            }
        }
    }
}

pub(crate) fn navigate(
    request: On<I3FocusRequest>,
    tree: FocusTree,
    focus: Res<FocusedWindow>,
    wrapping: Res<FocusWrapping>,
    mut pending: ResMut<TileCommands>,
    mut commands: Commands,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if tree.workspaces.is_empty() {
        let _ = pending.defer(*request.event());
        return;
    }
    if let Some(window) = focus
        .entity()
        .and_then(|window| tree.navigate(window, request.0, *wrapping))
    {
        commands.trigger(WindowCommand {
            window,
            kind: WindowCommandKind::Focus,
        });
        redraw.write(RequestRedraw);
    }
}

pub(crate) fn remember_focus(
    event: On<WindowFocusChanged>,
    tree: FocusTree,
    mut path: ResMut<FocusPath>,
) {
    tree.remember(event.window, &mut path);
}

pub(crate) fn refresh_path(
    tree: FocusTree,
    focus: Res<FocusedWindow>,
    changed: Query<(), Changed<TileParent>>,
    mut path: ResMut<FocusPath>,
) {
    if !changed.is_empty() {
        tree.remember(focus.entity(), &mut path);
    }
}

pub(crate) fn recover(
    tree: FocusTree,
    focus: Res<FocusedWindow>,
    path: Res<FocusPath>,
    windows: Query<&ManagedWindow>,
    mut commands: Commands,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let Some(lost) = focus.entity() else { return };
    if path.window != Some(lost) || windows.contains(lost) {
        return;
    }
    if let Some(window) = path
        .ancestors
        .iter()
        .find_map(|ancestor| tree.descend(*ancestor))
    {
        commands.trigger(WindowCommand {
            window,
            kind: WindowCommandKind::Focus,
        });
        redraw.write(RequestRedraw);
    }
}
