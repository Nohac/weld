//! Pointer resizing adjusts the adjacent branches at an existing split boundary.

use crate::{
    MAX_DEPTH, SplitAxis, TileContainer, TileParent, TileTreeChanged,
    layout::{LayoutDirty, LayoutRect, LayoutRequested},
};
use bevy::ecs::{
    component::Component,
    entity::Entity,
    observer::On,
    query::{With, Without},
    system::{Commands, Query, ResMut, SystemParam},
};
use weld_app::surface::ToplevelResizeEdge;
use weld_window::{
    WindowCommand, WindowCommandKind, WindowIntent, WindowIntentKind, WindowInteractionKind,
    WindowInteractionSession, WindowVisibility, fullscreen::WindowFullscreen,
    pointer::PointerInteractionRequest,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Boundary {
    parent: Entity,
    first: Entity,
    second: Entity,
}

#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TileResizeSession {
    horizontal: Option<Boundary>,
    vertical: Option<Boundary>,
}

#[derive(SystemParam)]
pub(crate) struct Boundaries<'w, 's> {
    parents: Query<'w, 's, &'static TileParent>,
    containers: Query<'w, 's, &'static TileContainer>,
}

impl Boundaries<'_, '_> {
    fn find(&self, mut branch: Entity, axis: SplitAxis, leading: bool) -> Option<Boundary> {
        for _ in 0..=MAX_DEPTH {
            let parent = self.parents.get(branch).ok()?.entity();
            let container = self.containers.get(parent).ok()?;
            let index = container
                .children
                .iter()
                .position(|child| child.entity == branch)?;
            if container.layout == crate::TileLayout::Split(axis) {
                let first = if leading {
                    index.checked_sub(1)
                } else {
                    Some(index)
                };
                if let Some(first) = first
                    && let Some(second) = container.children.get(first + 1)
                {
                    return Some(Boundary {
                        parent,
                        first: container.children[first].entity,
                        second: second.entity,
                    });
                }
            }
            branch = parent;
        }
        None
    }

    fn for_edges(&self, window: Entity, edges: ToplevelResizeEdge) -> TileResizeSession {
        TileResizeSession {
            horizontal: (edges.has_left() || edges.has_right())
                .then(|| self.find(window, SplitAxis::Horizontal, edges.has_left()))
                .flatten(),
            vertical: (edges.has_top() || edges.has_bottom())
                .then(|| self.find(window, SplitAxis::Vertical, edges.has_top()))
                .flatten(),
        }
    }
}

type ResizableTiles = (With<TileParent>, Without<WindowFullscreen>);

pub(crate) fn begin(
    event: On<PointerInteractionRequest>,
    windows: Query<(&WindowVisibility, Option<&WindowInteractionSession>), ResizableTiles>,
    boundaries: Boundaries,
    mut commands: Commands,
) {
    let WindowInteractionKind::Resize(edges) = event.kind else {
        return;
    };
    let Ok((WindowVisibility::Visible, None)) = windows.get(event.window) else {
        return;
    };
    let session = boundaries.for_edges(event.window, edges);
    if session.horizontal.is_none() && session.vertical.is_none() {
        return;
    }
    (*event.event()).accept(&mut commands, session);
}

type ActiveTiles<'w, 's> =
    Query<'w, 's, (&'static WindowVisibility, &'static WindowInteractionSession), ResizableTiles>;

pub(crate) fn validate_sessions(
    sessions: Query<(Entity, &TileResizeSession)>,
    windows: ActiveTiles,
    boundaries: Boundaries,
    mut commands: Commands,
) {
    for (window, session) in &sessions {
        let valid = windows.get(window).is_ok_and(|(visibility, interaction)| {
            *visibility == WindowVisibility::Visible && matches!(interaction.kind, WindowInteractionKind::Resize(edges) if boundaries.for_edges(window, edges) == *session)
        });
        if !valid {
            end(&mut commands, window);
        }
    }
}

fn end(commands: &mut Commands, window: Entity) {
    commands.entity(window).try_remove::<TileResizeSession>();
    commands.trigger(WindowCommand {
        window,
        kind: WindowCommandKind::EndInteraction,
    });
}

pub(crate) fn tree_changed(
    _event: On<TileTreeChanged>,
    sessions: Query<Entity, With<TileResizeSession>>,
    mut commands: Commands,
) {
    // Pruned removals are also covered by validate_sessions re-resolving edges.
    for window in &sessions {
        end(&mut commands, window);
    }
}

pub(crate) fn motion(
    event: On<WindowIntent>,
    sessions: Query<(&TileResizeSession, &WindowVisibility), ResizableTiles>,
    mut containers: Query<&mut TileContainer>,
    rectangles: Query<&LayoutRect>,
    mut dirty: ResMut<LayoutDirty>,
    mut commands: Commands,
) {
    if matches!(event.kind, WindowIntentKind::InteractionEnded(_)) {
        commands
            .entity(event.window)
            .try_remove::<TileResizeSession>();
        return;
    }
    let WindowIntentKind::ResizeBy(delta) = event.kind else {
        return;
    };
    if !delta.is_finite() {
        return;
    }
    let Ok((session, visibility)) = sessions.get(event.window) else {
        return;
    };
    if *visibility != WindowVisibility::Visible {
        end(&mut commands, event.window);
        return;
    }
    let mut changed = false;
    for (boundary, axis, movement) in [
        (session.horizontal, SplitAxis::Horizontal, delta.x),
        (session.vertical, SplitAxis::Vertical, delta.y),
    ] {
        let Some(boundary) = boundary else { continue };
        let Some(did_change) =
            resize_boundary(boundary, axis, movement, &mut containers, &rectangles)
        else {
            end(&mut commands, event.window);
            break;
        };
        changed |= did_change;
    }
    if changed {
        dirty.0 = true;
        commands.trigger(LayoutRequested);
    }
}

fn resize_boundary(
    boundary: Boundary,
    axis: SplitAxis,
    delta: f32,
    containers: &mut Query<&mut TileContainer>,
    rectangles: &Query<&LayoutRect>,
) -> Option<bool> {
    let mut container = containers.get_mut(boundary.parent).ok()?;
    if container.layout != crate::TileLayout::Split(axis) {
        return None;
    }
    let index = container
        .children
        .iter()
        .position(|child| child.entity == boundary.first)?;
    let second = container.children.get(index + 1)?;
    if second.entity != boundary.second {
        return None;
    }
    let first_size = rectangles.get(boundary.first).ok()?.0.size;
    let second_size = rectangles.get(boundary.second).ok()?.0.size;
    let span = match axis {
        SplitAxis::Horizontal => first_size.x + second_size.x,
        SplitAxis::Vertical => first_size.y + second_size.y,
    };
    if !span.is_finite() || span <= 0.0 || delta == 0.0 {
        return Some(false);
    }
    let first = container.children[index].weight;
    let total = first + second.weight;
    let share = (first / total + delta / span).clamp(0.05, 0.95);
    let next = total * share;
    if next == first {
        return Some(false);
    }
    container.children[index].weight = next;
    container.children[index + 1].weight = total - next;
    Some(true)
}
