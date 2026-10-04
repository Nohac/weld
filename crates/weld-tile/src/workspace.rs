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
    sole_tile: bool,
) -> WindowGeometry {
    let position = work_area.map_or(Vec2::ZERO, |area| {
        area.position.clamp(Vec2::ZERO, geometry.logical_size())
    });
    let size = work_area.map_or(geometry.logical_size(), |area| {
        area.size
            .max(Vec2::ZERO)
            .min(geometry.logical_size() - position)
    });
    let gap = if settings.hide_solo_gaps && sole_tile {
        0
    } else {
        settings.outer_gap
    };
    let margin = Vec2::splat(f32::from(gap)).min(size * 0.5);
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
            prepared_split: None,
            id,
            axis: settings.default_axis,
            layout: crate::TileLayout::Split(settings.default_axis),
            children: Vec::new(),
        },
        LayoutRect(bounds(geometry, work_area, settings, false)),
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
    let (source_workspace, floating) =
        if let Ok((owner, member, floating)) = windows.get(event.window) {
            if owner.0 != member.0 {
                return;
            }
            (member.0, floating.is_some())
        } else {
            let Some(root) = editor.root_of(event.window) else {
                return;
            };
            (root, false)
        };
    if !editor.roots.contains(source_workspace) || source_workspace == event.workspace {
        return;
    }
    let Ok((workspace, output)) = workspaces.get(event.workspace) else {
        return;
    };
    if floating {
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
            previous: source_workspace,
        });
        return;
    }
    if editor.root_of(event.window) != Some(source_workspace) {
        return;
    }
    let whole_workspace = event.window == source_workspace;
    let (source, source_index) = if whole_workspace {
        (source_workspace, None)
    } else {
        let Ok(parent) = editor.parents.get(event.window) else {
            return;
        };
        let source = parent.entity();
        let Ok(container) = editor.containers.get(source) else {
            return;
        };
        let Some(index) = container
            .children
            .iter()
            .position(|child| child.entity == event.window)
        else {
            return;
        };
        (source, Some(index))
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
    let mut destination_depth = 1;
    let mut ancestor = destination;
    while let Ok(parent) = editor.parents.get(ancestor) {
        destination_depth += 1;
        ancestor = parent.entity();
    }
    let mut leaves = Vec::new();
    let mut pending = vec![(event.window, destination_depth)];
    while let Some((node, depth)) = pending.pop() {
        if depth > crate::MAX_DEPTH {
            return;
        }
        if let Ok(container) = editor.containers.get(node) {
            for (child, _) in container.children() {
                if !editor
                    .parents
                    .get(child)
                    .is_ok_and(|parent| parent.entity() == node)
                {
                    return;
                }
                pending.push((child, depth + 1));
            }
        } else {
            if !windows.get(node).is_ok_and(|(owner, member, floating)| {
                owner.0 == source_workspace && member.0 == source_workspace && floating.is_none()
            }) {
                return;
            }
            leaves.push(node);
        }
    }
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
    if leaves.is_empty() || !weight.is_finite() || weight <= 0.0 {
        return;
    }
    // A workspace keeps its identity and output. Its selected tiling contents
    // become one ordinary split at the destination, retaining all inner edges.
    let moved = if whole_workspace {
        let Ok(container) = editor.containers.get(source) else {
            return;
        };
        let children = container.children.clone();
        let prepared_split = container.prepared_split;
        let axis = container.axis;
        let layout = container.layout;
        let Some(group) = editor.create_container(axis, children.clone()) else {
            return;
        };
        let recent = editor
            .history
            .recent()
            .find(|node| children.iter().any(|child| child.entity == *node));
        if let Some(recent) = recent {
            editor.history.wrap(recent, group);
        }
        editor
            .commands
            .entity(group)
            .entry::<TileContainer>()
            .and_modify(move |mut container| {
                container.layout = layout;
                container.prepared_split = prepared_split;
            });
        for child in children {
            editor
                .commands
                .entity(child.entity)
                .insert(TileParent(group));
        }
        if let Ok(mut container) = editor.containers.get_mut(source) {
            container.children.clear();
            container.prepared_split = None;
        }
        group
    } else {
        if let Some(index) = source_index
            && let Ok(mut container) = editor.containers.get_mut(source)
        {
            container.children.remove(index);
        }
        event.window
    };
    if let Ok(mut target) = editor.containers.get_mut(destination) {
        target.children.insert(
            insertion,
            TileChild {
                entity: moved,
                weight,
            },
        );
    }
    editor
        .commands
        .entity(moved)
        .insert(TileParent(destination));
    for leaf in leaves {
        editor.commands.entity(leaf).insert((
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
            window: leaf,
            previous: source_workspace,
        });
    }
    // Retain explicit unary splits, retiring only groups emptied by the move.
    editor.retire_empty_ancestors(source, source_workspace);
    editor.dirty.0 = true;
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
