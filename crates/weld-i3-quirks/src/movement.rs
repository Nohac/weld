//! i3 directional movement decisions over the shared split tree.
//!
//! The traversal follows i3's `tree_move` and `con_descend_direction` at the
//! revision recorded in ../UPSTREAM.md (attribution in ../LICENSE-i3).
//! Core applies each validated tree edit.

use bevy::ecs::{
    entity::Entity,
    event::Event,
    observer::On,
    system::{Commands, Query, Res, ResMut},
};
use weld_tile::{
    Direction, SplitAxis, TileCommands, TileContainer, TileParent, TileSelection, TileSide,
    TileTreeEdit,
};
use weld_window::FocusedWindow;

use crate::{I3MoveRequest, tree::TreeView};

#[derive(Event)]
pub(crate) struct AfterWrap {
    node: Entity,
    direction: Direction,
    root: Entity,
}

#[derive(Event)]
pub(crate) struct Cleanup {
    previous: Option<Entity>,
    moved: Option<(Entity, Entity)>,
}

enum MovePlan {
    Place { anchor: Entity, side: TileSide },
    Wrap { root: Entity, axis: SplitAxis },
}

fn axis(direction: Direction) -> SplitAxis {
    match direction {
        Direction::Left | Direction::Right => SplitAxis::Horizontal,
        Direction::Up | Direction::Down => SplitAxis::Vertical,
    }
}

fn forward(direction: Direction) -> bool {
    matches!(direction, Direction::Right | Direction::Down)
}

impl TreeView<'_, '_> {
    fn matching_parent(&self, mut branch: Entity, axis: SplitAxis) -> Option<(Entity, Entity)> {
        while let Ok(parent) = self.parents.get(branch) {
            let parent = parent.entity();
            if self.containers.get(parent).ok()?.axis() == axis {
                return Some((parent, branch));
            }
            branch = parent;
        }
        None
    }

    fn neighbor(&self, parent: Entity, child: Entity, forward: bool) -> Option<Entity> {
        let container = self.containers.get(parent).ok()?;
        let index = container.children().position(|(node, _)| node == child)?;
        let index = if forward {
            index.checked_add(1)?
        } else {
            index.checked_sub(1)?
        };
        container.children().nth(index).map(|(node, _)| node)
    }

    fn enter_branch(&self, mut branch: Entity, direction: Direction) -> Option<MovePlan> {
        while let Ok(container) = self.containers.get(branch) {
            branch = if container.axis() == axis(direction) {
                if forward(direction) {
                    container.children().next()
                } else {
                    container.children().last()
                }
                .map(|(child, _)| child)?
            } else {
                self.history
                    .recent()
                    .find(|child| {
                        self.parents
                            .get(*child)
                            .is_ok_and(|parent| parent.entity() == branch)
                    })
                    .or_else(|| container.children().next().map(|(child, _)| child))?
            };
        }
        let parent = self.parents.get(branch).ok()?.entity();
        let side =
            if self.containers.get(parent).ok()?.axis() != axis(direction) || !forward(direction) {
                TileSide::After
            } else {
                TileSide::Before
            };
        Some(MovePlan::Place {
            anchor: branch,
            side,
        })
    }

    fn move_plan(&self, node: Entity, direction: Direction, allow_wrap: bool) -> Option<MovePlan> {
        let root = self.root(node)?;
        let parent = self.parents.get(node).ok()?.entity();
        if parent == root && self.containers.get(root).ok()?.children().count() == 1 {
            return None;
        }
        let side = if forward(direction) {
            TileSide::After
        } else {
            TileSide::Before
        };
        let mut matching = self.matching_parent(node, axis(direction));
        if let Some((parent, branch)) = matching
            && branch == node
        {
            if let Some(next) = self.neighbor(parent, node, forward(direction)) {
                return if self.containers.contains(next) {
                    self.enter_branch(next, direction)
                } else {
                    Some(MovePlan::Place { anchor: next, side })
                };
            }
            // A workspace edge is a no-op until another output is available.
            if parent == root {
                return None;
            }
            matching = self.matching_parent(parent, axis(direction));
        }
        if let Some((parent, branch)) = matching {
            let next = self.neighbor(parent, branch, forward(direction));
            if let Some(next) = next
                && self.containers.contains(next)
            {
                return self.enter_branch(next, direction);
            }
            if next.is_none() {
                let immediate = self.parents.get(node).ok()?.entity();
                if self
                    .parents
                    .get(immediate)
                    .is_ok_and(|parent| parent.entity() == root)
                    && self.containers.get(immediate).ok()?.children().count() == 1
                {
                    return None;
                }
            }
            return Some(MovePlan::Place {
                anchor: branch,
                side,
            });
        }
        allow_wrap.then_some(MovePlan::Wrap {
            root,
            axis: axis(direction),
        })
    }
}

fn submit(
    plan: MovePlan,
    node: Entity,
    direction: Direction,
    tree: &TreeView,
    commands: &mut Commands,
) {
    match plan {
        MovePlan::Place { anchor, side } => {
            commands.trigger(TileTreeEdit::Place { node, anchor, side });
            if tree.parents.get(node).ok() != tree.parents.get(anchor).ok() {
                commands.trigger(Cleanup {
                    previous: None,
                    moved: tree
                        .parents
                        .get(anchor)
                        .ok()
                        .map(|parent| (node, parent.entity())),
                });
            }
        }
        MovePlan::Wrap { root, axis } => {
            commands.trigger(TileTreeEdit::WrapChildren {
                container: root,
                axis,
            });
            commands.trigger(AfterWrap {
                node,
                direction,
                root,
            });
        }
    }
}

pub(crate) fn move_focused(
    event: On<I3MoveRequest>,
    tree: TreeView,
    focus: Res<FocusedWindow>,
    selection: Res<TileSelection>,
    mut pending: ResMut<TileCommands>,
    mut commands: Commands,
) {
    if tree.workspaces.is_empty() {
        let _ = pending.defer(*event.event());
        return;
    }
    if let Some(node) = selection.target(&focus)
        && let Some(plan) = tree.move_plan(node, event.0, true)
    {
        submit(plan, node, event.0, &tree, &mut commands);
    }
}

pub(crate) fn after_wrap(event: On<AfterWrap>, tree: TreeView, mut commands: Commands) {
    // A rejected wrap (for example, at the depth limit) ends the operation.
    // Edits commit independently: if another observer invalidates the source
    // after a successful wrap, the coherent grouped workspace is retained.
    if tree
        .containers
        .get(event.root)
        .is_ok_and(|root| root.axis() == axis(event.direction))
        && let Some(plan) = tree.move_plan(event.node, event.direction, false)
    {
        submit(plan, event.node, event.direction, &tree, &mut commands);
    }
}

pub(crate) fn cleanup(
    event: On<Cleanup>,
    containers: Query<(Entity, &TileContainer)>,
    parents: Query<&TileParent>,
    mut commands: Commands,
) {
    // A rejected edit must terminate the continuation rather than retry it.
    if event.previous.is_some_and(|node| containers.contains(node)) {
        return;
    }
    if event.moved.is_some_and(|(node, expected)| {
        parents
            .get(node)
            .ok()
            .is_none_or(|parent| parent.entity() != expected)
    }) {
        return;
    }
    let redundant = containers
        .iter()
        .filter_map(|(node, container)| {
            if container.children().count() != 1 {
                return None;
            }
            let child = container.children().next()?.0;
            let (_, child_layout) = containers.get(child).ok()?;
            let parent = parents.get(node).ok()?.entity();
            let (_, parent_layout) = containers.get(parent).ok()?;
            (container.layout().is_split()
                && child_layout.layout().is_split()
                && parent_layout.layout().is_split()
                && container.axis() != child_layout.axis()
                && child_layout.axis() == parent_layout.axis())
            .then_some((container.id().raw(), node))
        })
        .min_by_key(|(id, _)| *id);
    if let Some((_, node)) = redundant {
        commands.trigger(TileTreeEdit::Flatten { container: node });
        commands.trigger(Cleanup {
            previous: Some(node),
            moved: None,
        });
    }
}
