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
    math::Vec2,
    picking::{
        events::{Click, Pointer, Press},
        pointer::PointerButton,
    },
    window::RequestRedraw,
};
use weld_app::output::{OutputGeometry, PrimaryOutput, WeldOutput};
use weld_window::{
    FocusedWindow, ManagedBy, ManagedWindow, WindowCloseHandle, WindowCommand, WindowCommandKind,
    WindowGeometry, WindowIntent, WindowIntentKind, WindowOutput, WindowProjectionLookup,
    WindowSystems, WindowVisibility, WindowZOrder,
};

use crate::{
    TileChild, TileCommands, TileFocusHistory, TileParent, TileSettings, TileState, TileSystems,
    TileWorkspace, history,
    layout::{self, LayoutDirty, LayoutRect},
    operations::{self, TreeEditor},
};

/// Installs split-tree admission, actions and layout before window presentation.
pub struct TilePlugin;

type ChangedOwnership = Or<(Changed<ManagedWindow>, Changed<ManagedBy>)>;
type Unmanaged = (Without<TileParent>, Without<ManagedBy>);

#[derive(QueryData)]
struct TiledWindow {
    entity: Entity,
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
            .add_observer(history::remember_focus)
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
                    ensure_workspace,
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

fn ensure_workspace(
    outputs: Query<&WeldOutput, With<PrimaryOutput>>,
    settings: Res<TileSettings>,
    mut editor: TreeEditor,
) {
    let Ok(output) = outputs.single() else { return };
    if editor.state.root.is_some() {
        return;
    }
    if let Some(root) = editor.create_container(settings.default_axis, Vec::new()) {
        editor
            .commands
            .entity(root)
            .insert(TileWorkspace { output: output.id });
        editor.state.root = Some(root);
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
    let Some(root) = editor.state.root else {
        return;
    };
    prune(&mut editor, &windows, root, root);
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
    windows: Query<(Entity, &ManagedWindow), Unmanaged>,
    outputs: Query<Entity, With<PrimaryOutput>>,
    focus: Res<FocusedWindow>,
    mut ordered: Local<Vec<(weld_window::WindowId, Entity)>>,
) {
    let Some(root) = editor.state.root else {
        return;
    };
    let Ok(output) = outputs.single() else { return };
    ordered.clear();
    ordered.extend(windows.iter().map(|(entity, window)| (window.id, entity)));
    if ordered.is_empty() {
        return;
    }
    ordered.sort_unstable_by_key(|(id, _)| *id);
    let focused = focus.entity();
    let parent = focused
        .and_then(|focused| editor.parents.get(focused).ok())
        .map_or(root, |parent| parent.0);
    let Ok(mut container) = editor.containers.get_mut(parent) else {
        return;
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
            WindowOutput(output),
            WindowVisibility::Visible,
        ));
        editor.commands.trigger(WindowCommand {
            window,
            kind: WindowCommandKind::Focus,
        });
    }
    editor.dirty.0 = true;
}

fn sync_output(
    outputs: Query<(Entity, &WeldOutput, &OutputGeometry), With<PrimaryOutput>>,
    settings: Res<TileSettings>,
    mut workspace: Query<(&mut TileWorkspace, &mut LayoutRect)>,
    windows: Query<TiledWindow, (With<ManagedWindow>, With<TileParent>)>,
    mut dirty: ResMut<LayoutDirty>,
    mut commands: Commands,
) {
    let Ok((output, identity, geometry)) = outputs.single() else {
        return;
    };
    let size = geometry.logical_size();
    let margin = Vec2::splat(f32::from(settings.outer_gap)).min(size * 0.5);
    let rect = WindowGeometry {
        position: margin,
        size: (size - 2.0 * margin).max(Vec2::ZERO),
    };
    for (mut workspace, mut bounds) in &mut workspace {
        if workspace.output != identity.id {
            workspace.output = identity.id;
        }
        if bounds.0 != rect {
            bounds.0 = rect;
            dirty.0 = true;
        }
    }
    if settings.is_changed() {
        dirty.0 = true;
    }
    for window in &windows {
        if window.output.is_none_or(|assigned| assigned.0 != output) {
            commands.entity(window.entity).insert(WindowOutput(output));
        }
        if *window.visibility != WindowVisibility::Visible {
            commands
                .entity(window.entity)
                .insert(WindowVisibility::Visible);
        }
        if *window.z_order != WindowZOrder(0) {
            commands.entity(window.entity).insert(WindowZOrder(0));
        }
    }
}

fn repair_focus(
    focus: Res<FocusedWindow>,
    windows: Query<(Entity, &ManagedWindow)>,
    parents: Query<(), With<TileParent>>,
    mut commands: Commands,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if focus
        .entity()
        .is_some_and(|entity| windows.contains(entity))
    {
        return;
    }
    if let Some((window, _)) = windows
        .iter()
        .filter(|(entity, _)| parents.contains(*entity))
        .min_by_key(|(_, window)| window.id)
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
    state: Res<TileState>,
    mut pending: ResMut<TileCommands>,
    mut commands: Commands,
) {
    if state.root.is_none() || pending.is_empty() {
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
