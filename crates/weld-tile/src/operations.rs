//! Ordered tree edits with component-scoped access and deferred publication.

use bevy::{
    ecs::{
        entity::Entity,
        message::MessageWriter,
        observer::On,
        query::With,
        system::{Commands, Query, Res, ResMut, SystemParam},
    },
    math::Vec2,
    window::RequestRedraw,
};
use weld_window::{FocusedWindow, ManagedWindow, WindowCommand, WindowCommandKind, WindowGeometry};

use crate::{
    ContainerId, Direction, SplitAxis, TileChild, TileCommands, TileContainer, TileFocusHistory,
    TileOperation, TileParent, TileRequest, TileState,
    layout::{LayoutDirty, LayoutRect, LayoutRequested},
};

#[derive(SystemParam)]
pub(crate) struct TreeEditor<'w, 's> {
    pub containers: Query<'w, 's, &'static mut TileContainer>,
    pub parents: Query<'w, 's, &'static mut TileParent>,
    pub state: ResMut<'w, TileState>,
    pub commands: Commands<'w, 's>,
    pub dirty: ResMut<'w, LayoutDirty>,
    pub history: ResMut<'w, TileFocusHistory>,
}

impl TreeEditor<'_, '_> {
    pub fn create_container(
        &mut self,
        axis: SplitAxis,
        children: Vec<TileChild>,
    ) -> Option<Entity> {
        let next = self.state.next_id.checked_add(1)?;
        let id = ContainerId(self.state.next_id);
        self.state.next_id = next;
        self.dirty.0 = true;
        Some(
            self.commands
                .spawn((TileContainer { id, axis, children }, LayoutRect::default()))
                .id(),
        )
    }

    fn split(&mut self, window: Entity, axis: SplitAxis) {
        let Ok(parent) = self.parents.get(window).copied() else {
            return;
        };
        let Ok(container) = self.containers.get(parent.0) else {
            return;
        };
        if container.children.len() == 1 {
            if let Ok(mut container) = self.containers.get_mut(parent.0)
                && container.axis != axis
            {
                container.axis = axis;
                self.dirty.0 = true;
            }
            return;
        }
        // Bound recursion for future IPC producers as well as local bindings.
        let mut depth = 1;
        let mut ancestor = parent.0;
        while let Ok(parent) = self.parents.get(ancestor) {
            depth += 1;
            ancestor = parent.0;
        }
        if depth >= 64 {
            return;
        }
        let Some(nested) = self.create_container(
            axis,
            vec![TileChild {
                entity: window,
                weight: 1.0,
            }],
        ) else {
            return;
        };
        // The new container is published before the following layout event.
        self.history.wrap(window, nested);
        self.commands.entity(nested).insert(parent);
        if let Ok(mut container) = self.containers.get_mut(parent.0)
            && let Some(child) = container
                .children
                .iter_mut()
                .find(|child| child.entity == window)
        {
            child.entity = nested;
        }
        if let Ok(mut parent) = self.parents.get_mut(window) {
            parent.0 = nested;
        }
    }

    fn swap(&mut self, first: Entity, second: Entity) {
        let Ok(first_parent) = self.parents.get(first).copied() else {
            return;
        };
        let Ok(second_parent) = self.parents.get(second).copied() else {
            return;
        };
        if let Ok(mut container) = self.containers.get_mut(first_parent.0) {
            for edge in &mut container.children {
                if edge.entity == first {
                    edge.entity = second;
                } else if first_parent == second_parent && edge.entity == second {
                    edge.entity = first;
                }
            }
        }
        if first_parent != second_parent
            && let Ok(mut container) = self.containers.get_mut(second_parent.0)
            && let Some(edge) = container
                .children
                .iter_mut()
                .find(|edge| edge.entity == second)
        {
            edge.entity = first;
        }
        if let Ok(mut parent) = self.parents.get_mut(first) {
            *parent = second_parent;
        }
        if let Ok(mut parent) = self.parents.get_mut(second) {
            *parent = first_parent;
        }
        self.dirty.0 = true;
    }

    fn resize(&mut self, window: Entity, axis: SplitAxis, fraction: f32) {
        if !fraction.is_finite() || fraction == 0.0 {
            return;
        }
        let mut branch = window;
        while let Ok(parent) = self.parents.get(branch).copied() {
            let Ok(container) = self.containers.get(parent.0) else {
                return;
            };
            if container.axis == axis && container.children.len() > 1 {
                let Some(index) = container
                    .children
                    .iter()
                    .position(|edge| edge.entity == branch)
                else {
                    return;
                };
                let other = if index + 1 < container.children.len() {
                    index + 1
                } else {
                    index - 1
                };
                let total = container.children[index].weight + container.children[other].weight;
                let share = (container.children[index].weight / total + fraction).clamp(0.05, 0.95);
                if let Ok(mut container) = self.containers.get_mut(parent.0) {
                    container.children[index].weight = total * share;
                    container.children[other].weight = total * (1.0 - share);
                }
                self.dirty.0 = true;
                return;
            }
            branch = parent.0;
        }
    }
}

pub(crate) fn apply_request(
    event: On<TileRequest>,
    mut editor: TreeEditor,
    windows: Query<(Entity, &ManagedWindow, &WindowGeometry), With<TileParent>>,
    focus: Res<FocusedWindow>,
    mut pending: ResMut<TileCommands>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if editor.state.root.is_none() {
        let _ = pending.defer(*event.event());
        return;
    }
    let (window, operation) = match *event.event() {
        TileRequest::Window(command) => (
            windows
                .iter()
                .find(|(_, window, _)| window.id == command.window)
                .map(|(entity, _, _)| entity),
            command.operation,
        ),
        TileRequest::Focused(operation) => (
            focus.entity().filter(|entity| windows.contains(*entity)),
            operation,
        ),
    };
    let Some(window) = window else { return };
    match operation {
        TileOperation::Close => editor.commands.trigger(WindowCommand {
            window,
            kind: WindowCommandKind::CloseOccupant,
        }),
        TileOperation::Split(axis) => editor.split(window, axis),
        TileOperation::Focus(direction) => {
            if let Some(next) = neighbor(&windows, window, direction) {
                editor.commands.trigger(WindowCommand {
                    window: next,
                    kind: WindowCommandKind::Focus,
                });
                redraw.write(RequestRedraw);
            }
        }
        TileOperation::Move(direction) => {
            if let Some(next) = neighbor(&windows, window, direction) {
                editor.swap(window, next);
            }
        }
        TileOperation::Resize { axis, fraction } => editor.resize(window, axis, fraction),
    }
    editor.commands.trigger(LayoutRequested);
}

fn neighbor(
    windows: &Query<(Entity, &ManagedWindow, &WindowGeometry), With<TileParent>>,
    window: Entity,
    direction: Direction,
) -> Option<Entity> {
    let (_, _, geometry) = windows.get(window).ok()?;
    let center = geometry.position + geometry.size * 0.5;
    let unit = match direction {
        Direction::Left => Vec2::NEG_X,
        Direction::Right => Vec2::X,
        Direction::Up => Vec2::NEG_Y,
        Direction::Down => Vec2::Y,
    };
    windows
        .iter()
        .filter_map(|(entity, managed, rect)| {
            let delta = rect.position + rect.size * 0.5 - center;
            (entity != window && delta.dot(unit) > 0.5).then_some((
                entity,
                delta.length_squared(),
                managed.id,
            ))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1).then(left.2.cmp(&right.2)))
        .map(|(entity, _, _)| entity)
}
