//! Admission, management scheduling and interaction adapters.

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        change_detection::DetectChanges,
        entity::Entity,
        lifecycle::{Remove, RemovedComponents},
        message::MessageWriter,
        observer::On,
        query::{Changed, Has, Or, QueryData, With, Without},
        schedule::IntoScheduleConfigs,
        system::{Commands, Local, Query, Res, ResMut, SystemParam},
    },
    window::RequestRedraw,
};
use weld_app::output::{OutputGeometry, OutputWorkArea, WeldOutput};
use weld_app::surface::ClientToplevelParent;
use weld_window::workspace::{
    FocusedWorkspace, Workspace, WorkspaceMember, WorkspaceOutput, WorkspaceWindows,
};
use weld_window::{
    FloatingWindow, FocusedWindow, ManagedBy, ManagedWindow, SoleTiledWindow, WindowClientResolver,
    WindowCommand, WindowCommandKind, WindowGeometry, WindowIntent, WindowIntentKind, WindowOutput,
    WindowSplitEdge, WindowSystems, WindowVisibility, WindowZOrder,
};

use std::collections::HashMap;
use weld_window::pointer::{WindowPointerPlugin, WindowPointerSystems};

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
type Unmanaged = (With<ManagedWindow>, Without<TileParent>, Without<ManagedBy>);

#[derive(QueryData)]
struct TiledWindow {
    entity: Entity,
    member: &'static WorkspaceMember,
    owner: &'static ManagedBy,
    output: Option<&'static WindowOutput>,
    z_order: &'static WindowZOrder,
    floating: Option<&'static FloatingWindow>,
    sole_tile: Has<SoleTiledWindow>,
}

impl Plugin for TilePlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<WindowPointerPlugin>() {
            app.add_plugins(WindowPointerPlugin);
        }
        app.configure_sets(
            PreUpdate,
            TileSystems::LateLayout
                .after(WindowSystems::InteractionFinalize)
                .before(WindowSystems::FinalReconcile),
        );
        app.add_systems(
            PreUpdate,
            (sync_visibility, layout::request_layout)
                .chain()
                .in_set(TileSystems::LateLayout),
        );
        app.configure_sets(
            PreUpdate,
            WindowPointerSystems::Start
                .after(TileSystems::Prepare)
                .before(TileSystems::Actions),
        );
        app.init_resource::<TileSettings>()
            .init_resource::<crate::TilePresentationMetrics>()
            .init_resource::<crate::TileSelection>()
            .add_observer(crate::selection::select)
            .add_observer(crate::selection::commit)
            .add_observer(crate::selection::focused)
            .add_observer(crate::selection::workspace_focused)
            .add_observer(crate::selection::activated)
            .add_systems(
                PreUpdate,
                crate::selection::reconcile
                    .after(TileSystems::Layout)
                    .in_set(WindowSystems::Management),
            )
            .init_resource::<TileState>()
            .init_resource::<TileCommands>()
            .init_resource::<TileFocusHistory>()
            .init_resource::<LayoutDirty>()
            .add_observer(intent)
            .add_observer(clear_tile_hints)
            .add_observer(mark_tiled_window)
            .add_systems(
                PreUpdate,
                sync_split_edges
                    .after(TileSystems::Layout)
                    .in_set(WindowSystems::Management),
            )
            .add_observer(crate::resize::begin)
            .add_observer(crate::resize::motion)
            .add_observer(crate::resize::tree_changed)
            .add_systems(
                PreUpdate,
                crate::resize::validate_sessions
                    .after(TileSystems::Layout)
                    .in_set(WindowSystems::Management),
            )
            .add_observer(operations::apply_request)
            .add_observer(structural::apply_edit)
            .add_observer(structural::set_layout)
            .add_observer(workspace::created)
            .add_observer(workspace::move_window)
            .add_observer(workspace::removed)
            .add_observer(crate::floating::request)
            .add_observer(crate::floating::cancel_centering)
            .add_observer(history::remember_focus)
            .add_observer(history::remember_tree_change)
            .add_systems(
                PreUpdate,
                sync_group_headers
                    .after(TileSystems::Layout)
                    .in_set(WindowSystems::Management),
            )
            .add_observer(layout::apply_layout)
            .add_systems(
                PreUpdate,
                classify_dialogs
                    .after(WindowSystems::Admission)
                    .before(WindowSystems::PresentationRevoke),
            )
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
                    sync_visibility,
                    repair_focus,
                    layout::request_layout,
                )
                    .chain()
                    .in_set(TileSystems::Prepare),
            )
            .add_systems(PreUpdate, drain_commands.in_set(TileSystems::Commands))
            .add_systems(
                PreUpdate,
                crate::floating::center_dialogs
                    .after(TileSystems::Layout)
                    .in_set(WindowSystems::Management),
            )
            .add_systems(
                PreUpdate,
                (
                    sync_output,
                    history::refresh_path,
                    sync_visibility,
                    layout::request_layout,
                )
                    .chain()
                    .in_set(TileSystems::Layout),
            );
    }
}

fn sync_split_edges(
    windows: Query<(Entity, &TileParent, Option<&WindowSplitEdge>), With<ManagedWindow>>,
    mut containers: Query<&mut crate::TileContainer>,
    mut commands: Commands,
) {
    for mut container in &mut containers {
        if let Some(window) = container.prepared_split
            && (container.children.len() != 1 || container.children[0].entity != window)
        {
            container.prepared_split = None;
        }
    }
    for (window, parent, current) in &windows {
        let edge = containers
            .get(parent.entity())
            .ok()
            .filter(|container| container.prepared_split == Some(window))
            .map(|container| match container.axis() {
                crate::SplitAxis::Horizontal => WindowSplitEdge::Right,
                crate::SplitAxis::Vertical => WindowSplitEdge::Bottom,
            });
        if current.copied() == edge {
            continue;
        }
        if let Some(edge) = edge {
            commands.entity(window).insert(edge);
        } else {
            commands.entity(window).remove::<WindowSplitEdge>();
        }
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
    let preserve_unary = !container.layout().is_split();
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
    if entity != root && (kept.is_empty() || (kept.len() == 1 && !preserve_unary)) {
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

#[derive(SystemParam)]
struct AdmissionFamilies<'w, 's> {
    clients: WindowClientResolver<'w, 's>,
    parents: Query<'w, 's, &'static ClientToplevelParent>,
    geometry: Query<'w, 's, &'static WindowGeometry>,
    memberships: Query<'w, 's, &'static WorkspaceMember>,
    floating: Query<'w, 's, (), With<FloatingWindow>>,
}

impl AdmissionFamilies<'_, '_> {
    fn parent(&self, window: Entity) -> Option<Entity> {
        let client = self.clients.client_entity(window)?;
        let parent = self.parents.get(client).ok()?;
        self.clients.window_for_surface(parent.surface)
    }
    fn is_dialog(&self, window: Entity) -> bool {
        self.clients
            .client_entity(window)
            .is_some_and(|client| self.parents.contains(client))
            || self
                .clients
                .mapped_client(window)
                .is_some_and(|client| client.hints().prefers_floating())
    }
}

fn classify_dialogs(
    windows: Query<Entity, Unmanaged>,
    families: AdmissionFamilies,
    mut commands: Commands,
) {
    for window in &windows {
        if families.is_dialog(window) && !families.floating.contains(window) {
            commands
                .entity(window)
                .insert((FloatingWindow, crate::floating::PendingDialogPlacement));
        }
    }
}

#[derive(SystemParam)]
struct AdmissionContext<'w> {
    workspace: Res<'w, FocusedWorkspace>,
    focus: Res<'w, FocusedWindow>,
    selection: Res<'w, crate::TileSelection>,
}

fn admit_windows(
    mut editor: TreeEditor,
    windows: Query<(Entity, &ManagedWindow, Option<&WorkspaceMember>), Unmanaged>,
    workspaces: Query<(Entity, &Workspace, &WorkspaceOutput), With<TileWorkspace>>,
    context: AdmissionContext,
    families: AdmissionFamilies,
    mut ordered: Local<Vec<(weld_window::WindowId, Entity)>>,
) {
    let selected = context.workspace.entity();
    for (root, workspace, output) in &workspaces {
        ordered.clear();
        ordered.extend(
            windows
                .iter()
                .filter(|(window, _, member)| {
                    member
                        .or_else(|| {
                            families
                                .parent(*window)
                                .and_then(|parent| families.memberships.get(parent).ok())
                        })
                        .map_or(selected == Some(root), |member| member.0 == root)
                })
                .map(|(entity, window, _)| (window.id, entity)),
        );
        if ordered.is_empty() {
            continue;
        }
        ordered.sort_unstable_by_key(|(id, _)| *id);
        let focused = context
            .selection
            .container()
            .filter(|node| editor.root_of(*node) == Some(root))
            .or_else(|| {
                context.focus.entity().filter(|window| {
                    families
                        .memberships
                        .get(*window)
                        .is_ok_and(|member| member.0 == root)
                })
            })
            .or_else(|| {
                workspace.recent().find(|window| {
                    families
                        .memberships
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
        let mut offset = 0;
        for (_, window) in ordered.iter().copied() {
            if families.is_dialog(window) || families.floating.contains(window) {
                let initial = families.geometry.get(window).copied().unwrap_or_default();
                let size = initial.size.max(bevy::math::Vec2::ONE);
                editor.commands.entity(window).insert((
                    FloatingWindow,
                    WindowGeometry {
                        position: initial.position,
                        size,
                    },
                    WindowZOrder(1),
                ));
                if !families.floating.contains(window) {
                    editor
                        .commands
                        .entity(window)
                        .insert(crate::floating::PendingDialogPlacement);
                }
            } else {
                container.children.insert(
                    insertion + offset,
                    TileChild {
                        entity: window,
                        weight: 1.0,
                    },
                );
                offset += 1;
                editor
                    .commands
                    .entity(window)
                    .insert((TileParent(parent), LayoutRect::default()));
            }
            editor.commands.entity(window).insert((
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

fn clear_tile_hints(event: On<Remove, TileParent>, mut commands: Commands) {
    commands.entity(event.entity).try_remove::<(
        SoleTiledWindow,
        WindowSplitEdge,
        weld_window::TiledWindow,
        weld_window::WindowGroupHeader,
    )>();
}

fn mark_tiled_window(
    event: On<bevy::ecs::lifecycle::Add, TileParent>,
    windows: Query<(), With<ManagedWindow>>,
    mut commands: Commands,
) {
    if windows.contains(event.entity) {
        commands
            .entity(event.entity)
            .insert(weld_window::TiledWindow);
    }
}

type WorkspaceLayouts<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Workspace,
        Option<&'static WorkspaceOutput>,
        Option<&'static WorkspaceWindows>,
        &'static mut LayoutRect,
    ),
    With<TileWorkspace>,
>;

fn sync_output(
    outputs: Query<(&OutputGeometry, Option<&OutputWorkArea>), With<WeldOutput>>,
    settings: Res<TileSettings>,
    mut workspaces: WorkspaceLayouts,
    windows: Query<TiledWindow, With<ManagedWindow>>,
    mut dirty: ResMut<LayoutDirty>,
    mut commands: Commands,
    mut sole_tiles: Local<HashMap<Entity, Option<Entity>>>,
) {
    sole_tiles.clear();
    for (workspace, _, output, members, mut bounds) in &mut workspaces {
        let mut tiles = members
            .into_iter()
            .flat_map(WorkspaceWindows::iter)
            .filter(|window| {
                windows
                    .get(*window)
                    .is_ok_and(|window| window.owner.0 == workspace && window.floating.is_none())
            });
        let sole = tiles.next().filter(|_| tiles.next().is_none());
        sole_tiles.insert(workspace, sole);
        let Some(output) = output else { continue };
        let Ok((geometry, work_area)) = outputs.get(output.0) else {
            continue;
        };
        let rect = workspace::bounds(geometry, work_area, &settings, sole.is_some());
        if bounds.0 != rect {
            bounds.0 = rect;
            dirty.0 = true;
        }
    }
    if settings.is_changed() {
        dirty.0 = true;
    }
    for window in &windows {
        let sole = window.owner.0 == window.member.0
            && sole_tiles.get(&window.member.0) == Some(&Some(window.entity));
        if sole != window.sole_tile {
            if sole {
                commands.entity(window.entity).insert(SoleTiledWindow);
            } else {
                commands.entity(window.entity).remove::<SoleTiledWindow>();
            }
        }
        if window.owner.0 != window.member.0 {
            continue;
        }
        let Ok((_, _, output, _, _)) = workspaces.get(window.member.0) else {
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
        if window.floating.is_none() && *window.z_order != WindowZOrder(0) {
            commands.entity(window.entity).insert(WindowZOrder(0));
        }
    }
}

#[derive(SystemParam)]
struct BranchVisibility<'w, 's> {
    parents: Query<'w, 's, &'static TileParent>,
    containers: Query<'w, 's, &'static crate::TileContainer>,
    history: Res<'w, TileFocusHistory>,
}

type VisibilityWindows<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static WorkspaceMember,
        &'static ManagedBy,
        &'static WindowVisibility,
        Has<FloatingWindow>,
    ),
>;

fn sync_visibility(
    windows: VisibilityWindows,
    workspaces: Query<(&Workspace, Option<&WorkspaceOutput>), With<TileWorkspace>>,
    outputs: Query<(), (With<WeldOutput>, With<OutputGeometry>)>,
    branches: BranchVisibility,
    mut commands: Commands,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    // Workspace commands and admission publish initial member visibility.
    // Refine it after management and late picking, before UI/focus/activity
    // publication. New late workspace mutations must precede LateLayout too.
    for (window, member, owner, current, floating) in &windows {
        if owner.0 != member.0 {
            continue;
        }
        let Ok((workspace, output)) = workspaces.get(member.0) else {
            continue;
        };
        let visible = workspace.visible()
            && output.is_some_and(|output| outputs.contains(output.0))
            && (floating
                || crate::visibility::branch_visible(
                    window,
                    &branches.parents,
                    &branches.containers,
                    &branches.history,
                ));
        let visibility = if visible {
            WindowVisibility::Visible
        } else {
            WindowVisibility::Hidden
        };
        if *current != visibility {
            commands.entity(window).insert(visibility);
            redraw.write(RequestRedraw);
        }
    }
}

fn sync_group_headers(
    windows: Query<
        (Entity, &LayoutRect, Option<&weld_window::WindowGroupHeader>),
        With<ManagedWindow>,
    >,
    parents: Query<&TileParent>,
    containers: Query<(&crate::TileContainer, &LayoutRect)>,
    settings: Res<crate::TilePresentationMetrics>,
    mut commands: Commands,
) {
    for (window, rect, current) in &windows {
        let mut node = window;
        let mut outer = None;
        for _ in 0..crate::MAX_DEPTH {
            let Ok(parent) = parents.get(node) else { break };
            node = parent.entity();
            let Ok((container, bounds)) = containers.get(node) else {
                break;
            };
            if !container.layout().is_split() {
                outer = Some(bounds.0);
            }
        }
        let grouped = outer.map(|bounds| {
            let border = f32::from(settings.group_border).min(bounds.size.min_element() * 0.5);
            let bottom = (rect.0.position.y + rect.0.size.y
                - (bounds.position.y + bounds.size.y - border))
                .abs()
                < 0.01;
            weld_window::WindowGroupHeader {
                bottom_left: bottom
                    && (rect.0.position.x - bounds.position.x - border).abs() < 0.01,
                bottom_right: bottom
                    && (rect.0.position.x + rect.0.size.x
                        - (bounds.position.x + bounds.size.x - border))
                        .abs()
                        < 0.01,
            }
        });
        if current.copied() == grouped {
            continue;
        }
        if let Some(grouped) = grouped {
            commands.entity(window).insert(grouped);
        } else {
            commands
                .entity(window)
                .remove::<weld_window::WindowGroupHeader>();
        }
    }
}

fn repair_focus(
    focus: Res<FocusedWindow>,
    windows: Query<(Entity, &ManagedWindow, &WorkspaceMember)>,
    selected: Res<FocusedWorkspace>,
    parents: Query<(), With<TileParent>>,
    workspaces: Query<&Workspace>,
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
    let recent = selected
        .entity()
        .and_then(|workspace| workspaces.get(workspace).ok())
        .and_then(|workspace| {
            workspace.recent().find(|window| {
                windows
                    .get(*window)
                    .is_ok_and(|(_, _, member)| Some(member.0) == selected.entity())
            })
        });
    let fallback = windows
        .iter()
        .filter(|(entity, _, member)| {
            parents.contains(*entity) && Some(member.0) == selected.entity()
        })
        .min_by_key(|(_, window, _)| window.id)
        .map(|(window, _, _)| window);
    if let Some(window) = recent.or(fallback) {
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
