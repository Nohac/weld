//! Admission, management scheduling and interaction adapters.

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        change_detection::DetectChanges,
        entity::Entity,
        lifecycle::RemovedComponents,
        message::MessageWriter,
        observer::On,
        query::{Changed, Or, QueryData, With, Without},
        schedule::IntoScheduleConfigs,
        system::{Commands, Local, Query, Res, ResMut},
    },
    picking::{
        events::{Click, Pointer, Press},
        pointer::PointerButton,
    },
    window::RequestRedraw,
};
use weld_app::output::{OutputGeometry, WeldOutput};
use weld_window::workspace::{FocusedWorkspace, Workspace, WorkspaceMember, WorkspaceOutput};
use weld_window::{
    FocusedWindow, ManagedBy, ManagedWindow, WindowCloseHandle, WindowCommand, WindowCommandKind,
    WindowIntent, WindowIntentKind, WindowOutput, WindowProjectionLookup, WindowSystems,
    WindowVisibility, WindowZOrder,
};

use crate::{
    TileChild, TileCommands, TileFocusHistory, TileParent, TileSettings, TileState, TileSystems,
    TileWorkspace, history,
    layout::{self, LayoutDirty, LayoutRect},
    operations::{self, TreeEditor},
    structural, workspace,
};

/// Installs split-tree admission, actions and layout before window presentation.
pub struct TilePlugin;

type ChangedOwnership = Or<(Changed<ManagedWindow>, Changed<ManagedBy>)>;
type Unmanaged = (Without<TileParent>, Without<ManagedBy>);

#[derive(QueryData)]
struct TiledWindow {
    entity: Entity,
    member: &'static WorkspaceMember,
    output: Option<&'static WindowOutput>,
    visibility: &'static WindowVisibility,
    z_order: &'static WindowZOrder,
}

impl Plugin for TilePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TileSettings>()
            .init_resource::<TileState>()
            .init_resource::<TileCommands>()
            .init_resource::<TileFocusHistory>()
            .init_resource::<LayoutDirty>()
            .add_observer(intent)
            .add_observer(activate)
            .add_observer(close)
            .add_observer(operations::apply_request)
            .add_observer(structural::apply_edit)
            .add_observer(workspace::created)
            .add_observer(workspace::move_window)
            .add_observer(workspace::removed)
            .add_observer(history::remember_focus)
            .add_observer(history::remember_tree_change)
            .add_observer(layout::apply_layout)
            .configure_sets(
                PreUpdate,
                (
                    TileSystems::RecoverFocus,
                    TileSystems::Prepare,
                    TileSystems::Commands,
                    TileSystems::Actions,
                    TileSystems::Layout,
                )
                    .chain()
                    .in_set(WindowSystems::Management),
            )
            .add_systems(
                PreUpdate,
                (
                    workspace::ensure_roots,
                    prune_removed,
                    admit_windows,
                    sync_output,
                    repair_focus,
                    layout::request_layout,
                )
                    .chain()
                    .in_set(TileSystems::Prepare),
            )
            .add_systems(PreUpdate, drain_commands.in_set(TileSystems::Commands))
            .add_systems(
                PreUpdate,
                (sync_output, layout::request_layout, history::refresh_path)
                    .chain()
                    .in_set(TileSystems::Layout),
            );
    }
}

fn prune_removed(
    mut editor: TreeEditor,
    windows: Query<(Option<&ManagedWindow>, Option<&ManagedBy>)>,
    changed: Query<(), ChangedOwnership>,
    mut removed_windows: RemovedComponents<ManagedWindow>,
    mut removed_managers: RemovedComponents<ManagedBy>,
) {
    let removed = removed_windows.read().count() + removed_managers.read().count();
    if removed == 0 && changed.is_empty() {
        return;
    }
    let roots: Vec<_> = editor.roots.iter().collect();
    for root in roots {
        prune(&mut editor, &windows, root, root);
    }
}

fn prune(
    editor: &mut TreeEditor,
    windows: &Query<(Option<&ManagedWindow>, Option<&ManagedBy>)>,
    entity: Entity,
    root: Entity,
) -> (Option<Entity>, bool) {
    let Ok(container) = editor.containers.get(entity) else {
        let live = windows.get(entity).is_ok_and(|(window, manager)| {
            window.is_some() && manager.is_some_and(|manager| manager.0 == root)
        });
        if !live {
            editor.history.replace(entity, None);
            editor
                .commands
                .entity(entity)
                .try_remove::<(TileParent, LayoutRect)>();
        }
        return (live.then_some(entity), !live);
    };
    let children = container.children.clone();
    let mut kept = Vec::with_capacity(children.len());
    let mut removed = false;
    for child in children {
        let (replacement, changed) = prune(editor, windows, child.entity, root);
        removed |= changed;
        if let Some(replacement) = replacement {
            if replacement != child.entity {
                editor
                    .commands
                    .entity(replacement)
                    .insert(TileParent(entity));
            }
            kept.push(TileChild {
                entity: replacement,
                ..child
            });
        }
    }
    if !removed {
        return (Some(entity), false);
    }
    editor.dirty.0 = true;
    if entity != root && kept.len() <= 1 {
        editor
            .history
            .replace(entity, kept.first().map(|child| child.entity));
        editor.commands.entity(entity).despawn();
        return (kept.first().map(|child| child.entity), true);
    }
    if let Ok(mut container) = editor.containers.get_mut(entity) {
        container.children = kept;
    }
    (Some(entity), true)
}

fn admit_windows(
    mut editor: TreeEditor,
    windows: Query<(Entity, &ManagedWindow, Option<&WorkspaceMember>), Unmanaged>,
    workspaces: Query<(Entity, &Workspace, &WorkspaceOutput), With<TileWorkspace>>,
    memberships: Query<&WorkspaceMember>,
    selected: Res<FocusedWorkspace>,
    focus: Res<FocusedWindow>,
    mut ordered: Local<Vec<(weld_window::WindowId, Entity)>>,
) {
    let selected = selected.entity();
    for (root, workspace, output) in &workspaces {
        ordered.clear();
        ordered.extend(
            windows
                .iter()
                .filter(|(_, _, member)| {
                    member.map_or(selected == Some(root), |member| member.0 == root)
                })
                .map(|(entity, window, _)| (window.id, entity)),
        );
        if ordered.is_empty() {
            continue;
        }
        ordered.sort_unstable_by_key(|(id, _)| *id);
        let focused = focus
            .entity()
            .filter(|window| {
                memberships
                    .get(*window)
                    .is_ok_and(|member| member.0 == root)
            })
            .or_else(|| {
                workspace.recent().find(|window| {
                    memberships
                        .get(*window)
                        .is_ok_and(|member| member.0 == root)
                })
            });
        let parent = focused
            .and_then(|focused| editor.parents.get(focused).ok())
            .map_or(root, |parent| parent.0);
        let Ok(mut container) = editor.containers.get_mut(parent) else {
            continue;
        };
        let insertion = focused
            .and_then(|focused| {
                container
                    .children
                    .iter()
                    .position(|child| child.entity == focused)
            })
            .map_or(container.children.len(), |index| index + 1);
        for (offset, (_, window)) in ordered.iter().copied().enumerate() {
            container.children.insert(
                insertion + offset,
                TileChild {
                    entity: window,
                    weight: 1.0,
                },
            );
            editor.commands.entity(window).insert((
                TileParent(parent),
                LayoutRect::default(),
                ManagedBy(root),
                WorkspaceMember(root),
                WindowOutput(output.0),
                if workspace.visible() {
                    WindowVisibility::Visible
                } else {
                    WindowVisibility::Hidden
                },
            ));
            if selected == Some(root) {
                editor.commands.trigger(WindowCommand {
                    window,
                    kind: WindowCommandKind::Focus,
                });
            }
        }
        editor.dirty.0 = true;
    }
}

fn sync_output(
    outputs: Query<&OutputGeometry, With<WeldOutput>>,
    settings: Res<TileSettings>,
    mut workspaces: Query<
        (&Workspace, Option<&WorkspaceOutput>, &mut LayoutRect),
        With<TileWorkspace>,
    >,
    windows: Query<TiledWindow, (With<ManagedWindow>, With<TileParent>)>,
    mut dirty: ResMut<LayoutDirty>,
    mut commands: Commands,
) {
    for (_, output, mut bounds) in &mut workspaces {
        let Some(output) = output else { continue };
        let Ok(geometry) = outputs.get(output.0) else {
            continue;
        };
        let rect = workspace::bounds(geometry, &settings);
        if bounds.0 != rect {
            bounds.0 = rect;
            dirty.0 = true;
        }
    }
    if settings.is_changed() {
        dirty.0 = true;
    }
    for window in &windows {
        let Ok((workspace, output, _)) = workspaces.get(window.member.0) else {
            continue;
        };
        let output = output.filter(|output| outputs.contains(output.0));
        if let Some(output) = output
            && window.output.is_none_or(|assigned| assigned.0 != output.0)
        {
            commands
                .entity(window.entity)
                .insert(WindowOutput(output.0));
        }
        let visibility = if output.is_some() && workspace.visible() {
            WindowVisibility::Visible
        } else {
            WindowVisibility::Hidden
        };
        if *window.visibility != visibility {
            commands.entity(window.entity).insert(visibility);
        }
        if *window.z_order != WindowZOrder(0) {
            commands.entity(window.entity).insert(WindowZOrder(0));
        }
    }
}

fn repair_focus(
    focus: Res<FocusedWindow>,
    windows: Query<(Entity, &ManagedWindow, &WorkspaceMember)>,
    selected: Res<FocusedWorkspace>,
    parents: Query<(), With<TileParent>>,
    mut commands: Commands,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if focus.entity().is_some_and(|entity| {
        windows
            .get(entity)
            .is_ok_and(|(_, _, member)| Some(member.0) == selected.entity())
    }) {
        return;
    }
    if let Some((window, _, _)) = windows
        .iter()
        .filter(|(entity, _, member)| {
            parents.contains(*entity) && Some(member.0) == selected.entity()
        })
        .min_by_key(|(_, window, _)| window.id)
    {
        commands.trigger(WindowCommand {
            window,
            kind: WindowCommandKind::Focus,
        });
        redraw.write(RequestRedraw);
    } else if let Some(window) = focus.entity() {
        commands.trigger(WindowCommand {
            window,
            kind: WindowCommandKind::ClearFocus,
        });
        redraw.write(RequestRedraw);
    }
}

fn drain_commands(
    roots: Query<(), With<TileWorkspace>>,
    mut pending: ResMut<TileCommands>,
    mut commands: Commands,
) {
    if roots.is_empty() || pending.is_empty() {
        return;
    }
    commands.append(&mut pending.queue);
    pending.count = 0;
}

fn intent(
    event: On<WindowIntent>,
    parents: Query<&TileParent>,
    mut commands: Commands,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if !parents.contains(event.window) {
        return;
    }
    let kind = match event.kind {
        WindowIntentKind::Activate => WindowCommandKind::Focus,
        WindowIntentKind::CloseRequested => WindowCommandKind::CloseOccupant,
        WindowIntentKind::MoveBy(_)
        | WindowIntentKind::ResizeBy(_)
        | WindowIntentKind::InteractionEnded(_) => return,
    };
    commands.trigger(WindowCommand {
        window: event.window,
        kind,
    });
    redraw.write(RequestRedraw);
}

fn activate(
    mut event: On<Pointer<Press>>,
    projections: WindowProjectionLookup,
    parents: Query<&TileParent>,
    mut commands: Commands,
) {
    if event.button != PointerButton::Primary {
        return;
    }
    if let Some(window) = projections
        .window_for(event.entity)
        .filter(|window| parents.contains(*window))
    {
        event.propagate(false);
        commands.trigger(WindowIntent {
            window,
            kind: WindowIntentKind::Activate,
        });
    }
}

fn close(
    mut event: On<Pointer<Click>>,
    projections: WindowProjectionLookup,
    handles: Query<(), With<WindowCloseHandle>>,
    parents: Query<&TileParent>,
    mut commands: Commands,
) {
    if event.button != PointerButton::Primary
        || !handles.contains(event.entity)
        || event.original_event_target() != event.entity
    {
        return;
    }
    if let Some(window) = projections
        .window_for(event.entity)
        .filter(|window| parents.contains(*window))
    {
        event.propagate(false);
        commands.trigger(WindowIntent {
            window,
            kind: WindowIntentKind::CloseRequested,
        });
    }
}
