//! Structural command selection and its managed-window highlight projection.

use crate::{TileContainer, TileFocusHistory, TileParent};
use bevy::{
    ecs::{
        entity::Entity,
        event::Event,
        message::MessageWriter,
        observer::On,
        query::With,
        resource::Resource,
        system::{Commands, Query, Res, ResMut},
    },
    window::RequestRedraw,
};
use weld_window::{
    FocusedWindow, ManagedBy, ManagedWindow, WindowCommand, WindowCommandKind, WindowFocusChanged,
    WindowGroupSelected, WindowIntent, WindowIntentKind,
    workspace::{FocusedWorkspace, WorkspaceFocused},
};

/// Explicit container selection. Leaf selection uses [`FocusedWindow`].
#[derive(Resource, Default, Debug)]
pub struct TileSelection(Option<Entity>);

impl TileSelection {
    pub fn container(&self) -> Option<Entity> {
        self.0
    }
    pub fn target(&self, focus: &FocusedWindow) -> Option<Entity> {
        self.0.or(focus.entity())
    }
}

/// Select a live tree node, retaining its most recently focused client leaf.
#[derive(Event, Clone, Copy, Debug)]
pub struct TileSelect(pub Entity);

#[derive(Event)]
pub(crate) struct SelectContainer(Entity);

pub(crate) fn root_of(node: Entity, parents: &Query<&TileParent>) -> Entity {
    let mut root = node;
    for _ in 0..crate::MAX_DEPTH {
        let Ok(parent) = parents.get(root) else { break };
        root = parent.entity();
    }
    root
}

pub(crate) fn contains(node: Entity, mut leaf: Entity, parents: &Query<&TileParent>) -> bool {
    for _ in 0..=crate::MAX_DEPTH {
        if leaf == node {
            return true;
        }
        let Ok(parent) = parents.get(leaf) else {
            return false;
        };
        leaf = parent.entity();
    }
    false
}

pub(crate) fn select(
    event: On<TileSelect>,
    containers: Query<&TileContainer>,
    parents: Query<&TileParent>,
    windows: Query<(Entity, &ManagedBy), With<ManagedWindow>>,
    selected: Res<FocusedWorkspace>,
    history: Res<TileFocusHistory>,
    mut commands: Commands,
) {
    let node = event.0;
    let root = root_of(node, &parents);
    if selected.entity() != Some(root) {
        return;
    }
    let mut leaf = node;
    for _ in 0..crate::MAX_DEPTH {
        let Ok(container) = containers.get(leaf) else {
            break;
        };
        let child = history
            .recent()
            .find(|child| {
                parents
                    .get(*child)
                    .is_ok_and(|parent| parent.entity() == leaf)
                    && container
                        .children()
                        .any(|(candidate, _)| candidate == *child)
            })
            .or_else(|| container.children().next().map(|(child, _)| child));
        let Some(child) = child else { return };
        leaf = child;
    }
    if !windows.get(leaf).is_ok_and(|(_, owner)| owner.0 == root) {
        return;
    }
    commands.trigger(WindowCommand {
        window: leaf,
        kind: WindowCommandKind::Focus,
    });
    // Focus observers finish before the structural selection is published.
    commands.trigger(SelectContainer(if containers.contains(node) {
        node
    } else {
        leaf
    }));
}

pub(crate) fn commit(
    event: On<SelectContainer>,
    containers: Query<(), With<TileContainer>>,
    parents: Query<&TileParent>,
    focus: Res<FocusedWindow>,
    mut selection: ResMut<TileSelection>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    selection.0 = containers
        .contains(event.0)
        .then_some(event.0)
        .filter(|node| {
            focus
                .entity()
                .is_some_and(|leaf| contains(*node, leaf, &parents))
        });
    redraw.write(RequestRedraw);
}

pub(crate) fn focused(_: On<WindowFocusChanged>, mut selection: ResMut<TileSelection>) {
    selection.0 = None;
}

pub(crate) fn workspace_focused(_: On<WorkspaceFocused>, mut selection: ResMut<TileSelection>) {
    selection.0 = None;
}

pub(crate) fn activated(event: On<WindowIntent>, mut selection: ResMut<TileSelection>) {
    if event.kind == WindowIntentKind::Activate {
        selection.0 = None;
    }
}

pub(crate) fn reconcile(
    mut selection: ResMut<TileSelection>,
    selected: Res<FocusedWorkspace>,
    focus: Res<FocusedWindow>,
    containers: Query<(), With<TileContainer>>,
    parents: Query<&TileParent>,
    windows: Query<(Entity, Option<&WindowGroupSelected>), With<ManagedWindow>>,
    mut commands: Commands,
) {
    if selection.0.is_some_and(|node| {
        !containers.contains(node)
            || selected.entity() != Some(root_of(node, &parents))
            || focus
                .entity()
                .is_none_or(|leaf| !contains(node, leaf, &parents))
    }) {
        selection.0 = None;
    }
    for (window, marked) in &windows {
        let highlighted = selection
            .0
            .is_some_and(|node| contains(node, window, &parents));
        if highlighted && marked.is_none() {
            commands.entity(window).insert(WindowGroupSelected);
        } else if !highlighted && marked.is_some() {
            commands.entity(window).remove::<WindowGroupSelected>();
        }
    }
}
