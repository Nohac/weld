//! Per-workspace tree initialization and atomic leaf transfers.

use bevy::ecs::{
    entity::Entity,
    lifecycle::Remove,
    observer::On,
    query::{With, Without},
    system::{Query, Res},
};
use bevy::math::Vec2;
use weld_app::output::{OutputGeometry, OutputWorkArea};
use weld_window::workspace::{
    Workspace, WorkspaceCreated, WorkspaceMember, WorkspaceMemberMoved, WorkspaceOutput,
};
use weld_window::{
    FloatingWindow, ManagedBy, ManagedWindow, WindowGeometry, WindowOutput, WindowVisibility,
};

use crate::{
    ContainerId, TileChild, TileContainer, TileParent, TileSettings, TileTreeChanged,
    TileWorkspace, TileWorkspaceMove,
    layout::{LayoutRect, LayoutRequested},
    operations::TreeEditor,
};

pub(crate) fn bounds(
    geometry: &OutputGeometry,
    work_area: Option<&OutputWorkArea>,
    settings: &TileSettings,
) -> WindowGeometry {
    let position = work_area.map_or(Vec2::ZERO, |area| {
        area.position.clamp(Vec2::ZERO, geometry.logical_size())
    });
    let size = work_area.map_or(geometry.logical_size(), |area| {
        area.size
            .max(Vec2::ZERO)
            .min(geometry.logical_size() - position)
    });
    let margin = Vec2::splat(f32::from(settings.outer_gap)).min(size * 0.5);
    WindowGeometry {
        position: position + margin,
        size: (size - 2.0 * margin).max(Vec2::ZERO),
    }
}

fn initialize(
    editor: &mut TreeEditor,
    workspace: Entity,
    settings: &TileSettings,
    geometry: &OutputGeometry,
    work_area: Option<&OutputWorkArea>,
) {
    if editor.roots.contains(workspace) {
        return;
    }
    let Some(next) = editor.state.next_id.checked_add(1) else {
        return;
    };
    let id = ContainerId(editor.state.next_id);
    editor.state.next_id = next;
    editor.commands.entity(workspace).insert((
        TileWorkspace,
        TileContainer {
            id,
            axis: settings.default_axis,
            children: Vec::new(),
        },
        LayoutRect(bounds(geometry, work_area, settings)),
    ));
    editor.dirty.0 = true;
}

pub(crate) fn created(
    event: On<WorkspaceCreated>,
    mut editor: TreeEditor,
    settings: Res<TileSettings>,
    workspaces: Query<&WorkspaceOutput, With<Workspace>>,
    outputs: Query<(&OutputGeometry, Option<&OutputWorkArea>)>,
) {
    if let Ok(output) = workspaces.get(event.0)
        && let Ok((geometry, work_area)) = outputs.get(output.0)
    {
        initialize(&mut editor, event.0, &settings, geometry, work_area);
    }
}

type UninitializedWorkspace = (With<Workspace>, Without<TileWorkspace>);

pub(crate) fn ensure_roots(
    mut editor: TreeEditor,
    settings: Res<TileSettings>,
    workspaces: Query<(Entity, &WorkspaceOutput), UninitializedWorkspace>,
    outputs: Query<(&OutputGeometry, Option<&OutputWorkArea>)>,
) {
    for (workspace, output) in &workspaces {
        if let Ok((geometry, work_area)) = outputs.get(output.0) {
            initialize(&mut editor, workspace, &settings, geometry, work_area);
        }
    }
}

pub(crate) fn move_window(
    event: On<TileWorkspaceMove>,
    mut editor: TreeEditor,
    windows: Query<(&ManagedBy, &WorkspaceMember, Option<&FloatingWindow>), With<ManagedWindow>>,
    workspaces: Query<(&Workspace, &WorkspaceOutput), With<TileWorkspace>>,
) {
    let Ok((owner, member, floating)) = windows.get(event.window) else {
        return;
    };
    if owner.0 != member.0 || !editor.roots.contains(owner.0) || member.0 == event.workspace {
        return;
    }
    let Ok((workspace, output)) = workspaces.get(event.workspace) else {
        return;
    };
    if floating.is_some() {
        editor
            .commands
            .entity(event.window)
            .remove::<(
                crate::floating::SavedTileSlot,
                crate::floating::PendingDialogPlacement,
            )>()
            .insert((
                WorkspaceMember(event.workspace),
                ManagedBy(event.workspace),
                WindowOutput(output.0),
                if workspace.visible() {
                    WindowVisibility::Visible
                } else {
                    WindowVisibility::Hidden
                },
            ));
        editor.commands.trigger(WorkspaceMemberMoved {
            window: event.window,
            previous: member.0,
        });
        return;
    }
    if editor.root_of(event.window) != Some(owner.0) {
        return;
    }
    let Ok(source) = editor.parents.get(event.window).copied() else {
        return;
    };
    let Ok(source_container) = editor.containers.get(source.entity()) else {
        return;
    };
    let Some(source_index) = source_container
        .children
        .iter()
        .position(|child| child.entity == event.window)
    else {
        return;
    };
    let (destination, insertion) = if let Some(anchor) = event.anchor {
        let Ok((owner, member, _)) = windows.get(anchor) else {
            return;
        };
        if owner.0 != event.workspace || member.0 != event.workspace {
            return;
        }
        if editor.root_of(anchor) != Some(event.workspace) {
            return;
        }
        let Ok(parent) = editor.parents.get(anchor) else {
            return;
        };
        let Ok(container) = editor.containers.get(parent.entity()) else {
            return;
        };
        let Some(index) = container
            .children
            .iter()
            .position(|child| child.entity == anchor)
        else {
            return;
        };
        (parent.entity(), index + 1)
    } else {
        let Ok(container) = editor.containers.get(event.workspace) else {
            return;
        };
        (event.workspace, container.children.len())
    };
    let Ok(target) = editor.containers.get(destination) else {
        return;
    };
    let weight = if target.children.is_empty() {
        1.0
    } else {
        target
            .children
            .iter()
            .map(|child| child.weight)
            .sum::<f32>()
            / target.children.len() as f32
    };
    if !weight.is_finite() || weight <= 0.0 {
        return;
    }
    if let Ok(mut source) = editor.containers.get_mut(source.entity()) {
        source.children.remove(source_index);
    }
    if let Ok(mut target) = editor.containers.get_mut(destination) {
        target.children.insert(
            insertion,
            TileChild {
                entity: event.window,
                weight,
            },
        );
    }
    editor.commands.entity(event.window).insert((
        TileParent(destination),
        WorkspaceMember(event.workspace),
        ManagedBy(event.workspace),
        WindowOutput(output.0),
        if workspace.visible() {
            WindowVisibility::Visible
        } else {
            WindowVisibility::Hidden
        },
    ));
    // Retain explicit unary splits, retiring only groups emptied by the move.
    editor.retire_empty_ancestors(source.entity(), owner.0);
    editor.dirty.0 = true;
    editor.commands.trigger(WorkspaceMemberMoved {
        window: event.window,
        previous: member.0,
    });
    editor.commands.trigger(TileTreeChanged);
    editor.commands.trigger(LayoutRequested);
}

pub(crate) fn removed(
    event: On<Remove, TileWorkspace>,
    mut editor: TreeEditor,
    owners: Query<(Entity, &ManagedBy)>,
) {
    for (window, owner) in &owners {
        if owner.0 == event.entity {
            editor.commands.entity(window).try_remove::<ManagedBy>();
        }
    }
    let mut pending = vec![event.entity];
    while let Some(node) = pending.pop() {
        if let Ok(container) = editor.containers.get(node) {
            pending.extend(container.children().map(|(child, _)| child));
            if node != event.entity {
                editor.commands.entity(node).despawn();
            }
        } else {
            editor
                .commands
                .entity(node)
                .try_remove::<(TileParent, LayoutRect)>();
        }
        editor.history.replace(node, None);
    }
}
