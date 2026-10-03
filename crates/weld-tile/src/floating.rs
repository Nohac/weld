//! Atomic transfers between a workspace's split tree and freeform plane.

use crate::{
    TileChild, TileFloatingRequest, TileParent, TileTreeChanged,
    layout::{LayoutRect, LayoutRequested},
    operations::TreeEditor,
};
use bevy::{
    ecs::{
        component::Component,
        entity::Entity,
        observer::On,
        query::{QueryData, With},
        system::{Commands, Query, Res},
    },
    math::Vec2,
};
use weld_app::surface::ClientToplevelParent;
use weld_window::{
    FloatingWindow, FocusedWindow, ManagedBy, ManagedWindow, WindowClientResolver, WindowCommand,
    WindowCommandKind, WindowGeometry, WindowIntent, WindowIntentKind, WindowZOrder,
    workspace::WorkspaceMember,
};

#[derive(Component, Clone, Copy)]
pub(crate) struct SavedTileSlot {
    parent: Entity,
    index: usize,
    weight: f32,
}

#[derive(Component, Clone, Copy)]
pub(crate) struct SavedFloatingGeometry(pub WindowGeometry);

#[derive(Component)]
pub(crate) struct PendingDialogPlacement;

type CenteredDialogs = (With<PendingDialogPlacement>, With<FloatingWindow>);

/// Center after initial parent layout, including simultaneously mapped families.
pub(crate) fn center_dialogs(
    pending: Query<(Entity, &WorkspaceMember), CenteredDialogs>,
    mut geometry: Query<&mut WindowGeometry>,
    rectangles: Query<&LayoutRect>,
    clients: WindowClientResolver,
    parents: Query<&ClientToplevelParent>,
    mut commands: Commands,
) {
    for (window, member) in &pending {
        let declared_parent = clients
            .client_entity(window)
            .and_then(|client| parents.get(client).ok());
        let parent = declared_parent.and_then(|parent| clients.window_for_surface(parent.surface));
        let bounds = parent
            .and_then(|parent| geometry.get(parent).ok())
            .copied()
            .or_else(|| rectangles.get(member.0).ok().map(|rect| rect.0));
        if let Some(bounds) = bounds
            && let Ok(mut geometry) = geometry.get_mut(window)
        {
            let position = bounds.position + (bounds.size - geometry.size) * 0.5;
            if geometry.position != position {
                geometry.position = position;
            }
        }
        if declared_parent.is_none() || parent.is_some() {
            commands.entity(window).remove::<PendingDialogPlacement>();
        }
    }
}

pub(crate) fn cancel_centering(event: On<WindowIntent>, mut commands: Commands) {
    if matches!(
        event.kind,
        WindowIntentKind::MoveBy(_) | WindowIntentKind::ResizeBy(_)
    ) {
        commands
            .entity(event.window)
            .try_remove::<PendingDialogPlacement>();
    }
}

#[derive(QueryData)]
#[query_data(mutable)]
pub(crate) struct FloatingTarget {
    owner: &'static ManagedBy,
    member: &'static WorkspaceMember,
    geometry: &'static mut WindowGeometry,
    floating: Option<&'static FloatingWindow>,
    slot: Option<&'static SavedTileSlot>,
    saved: Option<&'static SavedFloatingGeometry>,
}

pub(crate) fn request(
    event: On<TileFloatingRequest>,
    mut editor: TreeEditor,
    focus: Res<FocusedWindow>,
    mut windows: Query<FloatingTarget, With<ManagedWindow>>,
    rectangles: Query<&LayoutRect>,
) {
    let Some(window) = event.window.or(focus.entity()) else {
        return;
    };
    let Ok(FloatingTargetItem {
        owner,
        member,
        mut geometry,
        floating,
        slot,
        saved,
    }) = windows.get_mut(window)
    else {
        return;
    };
    if owner.0 != member.0 || !editor.roots.contains(owner.0) {
        return;
    }
    let enabled = event.enabled.unwrap_or(floating.is_none());
    if enabled == floating.is_some() {
        return;
    }
    editor
        .commands
        .entity(window)
        .remove::<PendingDialogPlacement>();
    if enabled {
        let Ok(parent) = editor.parents.get(window).copied() else {
            return;
        };
        let Ok(mut container) = editor.containers.get_mut(parent.entity()) else {
            return;
        };
        let Some(index) = container
            .children
            .iter()
            .position(|child| child.entity == window)
        else {
            return;
        };
        editor.commands.trigger(WindowCommand {
            window,
            kind: WindowCommandKind::EndInteraction,
        });
        let child = container.children.remove(index);
        let remembered = SavedTileSlot {
            parent: parent.entity(),
            index,
            weight: child.weight,
        };
        let freeform = saved.map(|saved| saved.0).unwrap_or_else(|| {
            let bounds = rectangles.get(owner.0).map_or(*geometry, |rect| rect.0);
            let size = geometry.size.min(bounds.size * 0.7).max(Vec2::ONE);
            WindowGeometry {
                position: bounds.position + (bounds.size - size) * 0.5,
                size,
            }
        });
        *geometry = freeform;
        editor
            .commands
            .entity(window)
            .remove::<(TileParent, LayoutRect)>()
            .insert((FloatingWindow, remembered, WindowZOrder(1)));
        editor.retire_empty_ancestors(parent.entity(), owner.0);
    } else {
        let destination = slot
            .filter(|slot| editor.root_of(slot.parent) == Some(owner.0))
            .map_or(owner.0, |slot| slot.parent);
        let Ok(mut container) = editor.containers.get_mut(destination) else {
            return;
        };
        editor.commands.trigger(WindowCommand {
            window,
            kind: WindowCommandKind::EndInteraction,
        });
        let index = slot
            .filter(|slot| slot.parent == destination)
            .map_or(container.children.len(), |slot| {
                slot.index.min(container.children.len())
            });
        let weight = slot.map_or(1.0, |slot| slot.weight);
        container.children.insert(
            index,
            TileChild {
                entity: window,
                weight,
            },
        );
        editor
            .commands
            .entity(window)
            .remove::<(FloatingWindow, SavedTileSlot)>()
            .insert((
                TileParent(destination),
                LayoutRect::default(),
                SavedFloatingGeometry(*geometry),
                WindowZOrder(0),
            ));
    }
    editor.dirty.0 = true;
    editor.commands.trigger(TileTreeChanged);
    editor.commands.trigger(LayoutRequested);
}
