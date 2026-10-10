//! Ordered tree edits with component-scoped access and deferred publication.

use bevy::{
    ecs::{
        entity::Entity,
        message::MessageWriter,
        observer::On,
        query::{Has, With},
        system::{Commands, Query, Res, ResMut, SystemParam},
    },
    math::Vec2,
    window::RequestRedraw,
};
use weld_window::{
    FocusedWindow, ManagedBy, ManagedWindow, WindowCommand, WindowCommandKind, WindowGeometry,
};

use crate::{
    ContainerId, Direction, SplitAxis, TileChild, TileCommands, TileContainer, TileFocusHistory,
    TileOperation, TileParent, TileRequest, TileState, TileTreeChanged, TileWorkspace,
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
    pub roots: Query<'w, 's, Entity, With<TileWorkspace>>,
    pub rects: Query<'w, 's, &'static LayoutRect>,
}

impl TreeEditor<'_, '_> {
    /// Retire emptied nested containers while preserving explicit unary splits.
    pub(crate) fn retire_empty_ancestors(&mut self, mut node: Entity, root: Entity) {
        while node != root {
            let Ok(container) = self.containers.get(node) else {
                break;
            };
            if !container.children.is_empty() {
                break;
            }
            let Ok(parent) = self.parents.get(node).copied() else {
                break;
            };
            if let Ok(mut container) = self.containers.get_mut(parent.entity()) {
                container.children.retain(|child| child.entity != node);
            }
            self.history.replace(node, None);
            self.commands.entity(node).despawn();
            node = parent.entity();
        }
    }
    /// Resolves a root while validating bounded, bidirectional ancestry.
    pub fn root_of(&self, mut node: Entity) -> Option<Entity> {
        for _ in 0..=crate::MAX_DEPTH {
            if self.roots.contains(node) {
                return Some(node);
            }
            let parent = self.parents.get(node).ok()?.entity();
            if !self
                .containers
                .get(parent)
                .ok()?
                .children()
                .any(|(child, _)| child == node)
            {
                return None;
            }
            node = parent;
        }
        None
    }

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
                .spawn((
                    TileContainer {
                        id,
                        axis,
                        layout: crate::TileLayout::Split(axis),
                        children,
                        prepared_split: None,
                    },
                    LayoutRect::default(),
                ))
                .id(),
        )
    }

    fn split(&mut self, window: Entity, axis: SplitAxis) {
        if self.roots.contains(window) {
            if let Ok(mut container) = self.containers.get_mut(window) {
                let prepared = container.prepared_split;
                container.set_layout(crate::TileLayout::Split(axis));
                container.prepared_split = prepared;
                self.dirty.0 = true;
            }
            return;
        }
        let Ok(parent) = self.parents.get(window).copied() else {
            return;
        };
        let Ok(container) = self.containers.get(parent.0) else {
            return;
        };
        if container.children.len() == 1 && container.layout.is_split() {
            if let Ok(mut container) = self.containers.get_mut(parent.0) {
                container.set_layout(crate::TileLayout::Split(axis));
                container.prepared_split = Some(window);
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
        let mut descendants = vec![(window, depth)];
        while let Some((node, depth)) = descendants.pop() {
            if depth >= crate::MAX_DEPTH {
                return;
            }
            if let Ok(container) = self.containers.get(node) {
                descendants.extend(container.children().map(|(child, _)| (child, depth + 1)));
            }
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
        self.commands
            .entity(nested)
            .entry::<TileContainer>()
            .and_modify(move |mut container| {
                container.prepared_split = Some(window);
            });
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

    fn resize(&mut self, window: Entity, axis: SplitAxis, amount: f32, pixels: bool) {
        if !amount.is_finite() || amount == 0.0 {
            return;
        }
        let mut branch = window;
        while let Ok(parent) = self.parents.get(branch).copied() {
            let Ok(container) = self.containers.get(parent.0) else {
                return;
            };
            if container.layout == crate::TileLayout::Split(axis) && container.children.len() > 1 {
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
                let fraction = if pixels {
                    let extent = [index, other]
                        .into_iter()
                        .filter_map(|index| self.rects.get(container.children[index].entity).ok())
                        .map(|rect| match axis {
                            SplitAxis::Horizontal => rect.0.size.x,
                            SplitAxis::Vertical => rect.0.size.y,
                        })
                        .sum::<f32>();
                    if extent <= 0.0 {
                        return;
                    }
                    amount / extent
                } else {
                    amount
                };
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
    windows: Query<(Entity, &ManagedWindow, &WindowGeometry)>,
    selection: CommandSelection,
    owners: Query<(&ManagedBy, Has<TileParent>)>,
    mut pending: ResMut<TileCommands>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if editor.roots.is_empty() {
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
        TileRequest::Focused(operation) => {
            (selection.selection.target(&selection.focus), operation)
        }
    };
    let Some(window) = window else { return };
    let container = editor.containers.contains(window);
    let valid = if container {
        editor.root_of(window).is_some()
    } else {
        owners.get(window).is_ok_and(|(owner, tiled)| {
            editor.roots.contains(owner.0) && (tiled || operation == TileOperation::Close)
        })
    };
    if !valid {
        return;
    }
    match operation {
        TileOperation::Close => {
            let mut pending = vec![window];
            while let Some(node) = pending.pop() {
                if let Ok(container) = editor.containers.get(node) {
                    pending.extend(container.children().map(|(child, _)| child));
                } else {
                    editor.commands.trigger(WindowCommand {
                        window: node,
                        kind: WindowCommandKind::CloseOccupant,
                    });
                }
            }
        }
        TileOperation::Split(axis) => editor.split(window, axis),
        TileOperation::Focus(direction) => {
            if let Some(next) = neighbor(&windows, &owners, &editor, window, direction) {
                editor.commands.trigger(WindowCommand {
                    window: next,
                    kind: WindowCommandKind::Focus,
                });
                redraw.write(RequestRedraw);
            }
        }
        TileOperation::Move(direction) => {
            if let Some(next) = neighbor(&windows, &owners, &editor, window, direction) {
                editor.swap(window, next);
            }
        }
        TileOperation::Resize { axis, fraction } => editor.resize(window, axis, fraction, false),
        TileOperation::ResizePixels { axis, pixels } => editor.resize(window, axis, pixels, true),
    }
    if editor.dirty.0 {
        editor.commands.trigger(TileTreeChanged);
    }
    editor.commands.trigger(LayoutRequested);
}

#[derive(SystemParam)]
pub(crate) struct CommandSelection<'w> {
    focus: Res<'w, FocusedWindow>,
    selection: Res<'w, crate::TileSelection>,
}

fn neighbor(
    windows: &Query<(Entity, &ManagedWindow, &WindowGeometry)>,
    owners: &Query<(&ManagedBy, Has<TileParent>)>,
    editor: &TreeEditor,
    window: Entity,
    direction: Direction,
) -> Option<Entity> {
    let (_, _, geometry) = windows.get(window).ok()?;
    let owner = owners.get(window).ok()?.0.0;
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
            if !crate::visibility::branch_visible(
                entity,
                &editor.parents.as_readonly(),
                &editor.containers.as_readonly(),
                &editor.history,
            ) || !owners
                .get(entity)
                .is_ok_and(|(candidate, tiled)| tiled && candidate.0 == owner)
            {
                return None;
            }
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
