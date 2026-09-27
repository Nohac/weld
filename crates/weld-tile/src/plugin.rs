//! Admission and interaction adapters for the shared window domain.

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        entity::Entity,
        message::{MessageWriter, Messages},
        observer::On,
        query::{With, Without},
        schedule::IntoScheduleConfigs,
        system::{Commands, Query},
        world::World,
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
    QueuedCommand, TileChild, TileCommands, TileContainer, TileOperation, TileParent, TileSettings,
    TileState, TileWorkspace, layout, operations,
};

/// Installs tiling policy without decorations, configuration, or input bindings.
pub struct TilePlugin;

impl Plugin for TilePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TileSettings>()
            .init_resource::<TileState>()
            .init_resource::<TileCommands>()
            .add_observer(intent)
            .add_observer(activate)
            .add_observer(close)
            .add_systems(PreUpdate, manage.in_set(WindowSystems::Management));
    }
}

// Improvement: express admission, edits and layout as scheduled systems with
// explicit Query/Res/ResMut access and a defined tree-edit publication boundary.
// Include TileCommands::flush and Master dispatch in this refactor so ordered
// cross-plugin actions flow through the schedule. See docs/master-tiling.md.
fn manage(world: &mut World) {
    let output = {
        let mut query =
            world.query_filtered::<(Entity, &WeldOutput, &OutputGeometry), With<PrimaryOutput>>();
        let mut outputs = query.iter(world);
        let first = outputs
            .next()
            .map(|(entity, output, geometry)| (entity, output.id, geometry.logical_size()));
        if outputs.next().is_some() {
            return;
        }
        first
    };
    let Some((output, output_id, size)) = output else {
        return;
    };
    let settings = *world.resource::<TileSettings>();
    let root = match world.resource::<TileState>().root {
        Some(root) => root,
        None => {
            let Some(root) = operations::create_container(world, settings.default_axis) else {
                return;
            };
            if let Ok(mut entity) = world.get_entity_mut(root) {
                entity.insert(TileWorkspace { output: output_id });
            }
            world.resource_mut::<TileState>().root = Some(root);
            root
        }
    };
    if world
        .get::<TileWorkspace>(root)
        .is_none_or(|workspace| workspace.output != output_id)
        && let Ok(mut entity) = world.get_entity_mut(root)
    {
        entity.insert(TileWorkspace { output: output_id });
    }
    prune(world, root, root);
    let mut new_windows: Vec<_> = world
        .query_filtered::<(Entity, &ManagedWindow, Option<&ManagedBy>), Without<TileParent>>()
        .iter(world)
        .filter(|(_, _, manager)| manager.is_none())
        .map(|(entity, window, _)| (window.id, entity))
        .collect();
    new_windows.sort_unstable_by_key(|(id, _)| *id);
    for (_, window) in new_windows {
        if world.get::<ManagedWindow>(window).is_none() {
            continue;
        }
        let focused = world.resource::<FocusedWindow>().entity();
        let parent = focused
            .and_then(|focused| world.get::<TileParent>(focused))
            .map_or(root, |parent| parent.0);
        if let Some(mut container) = world.get_mut::<TileContainer>(parent) {
            let index = focused
                .and_then(|focused| {
                    container
                        .children
                        .iter()
                        .position(|child| child.entity == focused)
                })
                .map_or(container.children.len(), |index| index + 1);
            container.children.insert(
                index,
                TileChild {
                    entity: window,
                    weight: 1.0,
                },
            );
        }
        if let Ok(mut entity) = world.get_entity_mut(window) {
            entity.insert((
                TileParent(parent),
                ManagedBy(root),
                WindowOutput(output),
                WindowVisibility::Visible,
            ));
        }
        operations::focus(world, window);
    }
    let mut windows: Vec<_> = world
        .query::<(Entity, &TileParent, &ManagedWindow)>()
        .iter(world)
        .map(|(entity, _, window)| (window.id, entity))
        .collect();
    windows.sort_unstable_by_key(|(id, _)| *id);
    for (_, window) in &windows {
        if world
            .get::<WindowOutput>(*window)
            .is_none_or(|assigned| assigned.0 != output)
            && let Ok(mut entity) = world.get_entity_mut(*window)
        {
            entity.insert(WindowOutput(output));
        }
        // This manager has only tiled leaves; all remain visible, including
        // retained vacancies and source-side remote placeholders.
        if world.get::<WindowVisibility>(*window) != Some(&WindowVisibility::Visible)
            && let Ok(mut entity) = world.get_entity_mut(*window)
        {
            entity.insert(WindowVisibility::Visible);
        }
        if world.get::<WindowZOrder>(*window) != Some(&WindowZOrder(0))
            && let Ok(mut entity) = world.get_entity_mut(*window)
        {
            entity.insert(WindowZOrder(0));
        }
    }
    if world
        .resource::<FocusedWindow>()
        .entity()
        .is_none_or(|focused| world.get::<ManagedWindow>(focused).is_none())
    {
        if let Some((_, window)) = windows.first() {
            operations::focus(world, *window);
        } else if let Some(window) = world.resource::<FocusedWindow>().entity() {
            world.trigger(WindowCommand {
                window,
                kind: WindowCommandKind::ClearFocus,
            });
        }
    }
    let margin = Vec2::splat(f32::from(settings.outer_gap)).min(size * 0.5);
    let rect = WindowGeometry {
        position: margin,
        size: (size - 2.0 * margin).max(Vec2::ZERO),
    };
    let gap = f32::from(settings.inner_gap);
    if layout::arrange(world, root, rect, gap) {
        world
            .resource_mut::<Messages<RequestRedraw>>()
            .write(RequestRedraw);
    }
    flush_commands(world);
}

pub(crate) fn flush_commands(world: &mut World) {
    if world
        .get_resource::<TileCommands>()
        .is_none_or(|commands| commands.0.is_empty())
    {
        return;
    }
    let Some(root) = world
        .get_resource::<TileState>()
        .and_then(|state| state.root)
    else {
        return;
    };
    let Some(rect) = world.get::<layout::LayoutRect>(root).map(|rect| rect.0) else {
        return;
    };
    let gap = f32::from(world.resource::<TileSettings>().inner_gap);
    prune(world, root, root);
    let mut changed = layout::arrange(world, root, rect, gap);
    while let Some(command) = world.resource_mut::<TileCommands>().0.pop_front() {
        let (window, operation) = match command {
            QueuedCommand::Window(command) => {
                let window = world
                    .query::<(Entity, &ManagedWindow, &TileParent)>()
                    .iter(world)
                    .find(|(_, managed, _)| managed.id == command.window)
                    .map(|(entity, _, _)| entity);
                (window, command.operation)
            }
            QueuedCommand::Focused(operation) => (
                world
                    .resource::<FocusedWindow>()
                    .entity()
                    .filter(|window| world.get::<TileParent>(*window).is_some()),
                operation,
            ),
        };
        let Some(window) = window else {
            continue;
        };
        let previous_focus = world.resource::<FocusedWindow>().entity();
        match operation {
            TileOperation::Close => world.trigger(WindowCommand {
                window,
                kind: WindowCommandKind::CloseOccupant,
            }),
            TileOperation::Split(axis) => operations::split(world, window, axis),
            TileOperation::Focus(direction) => {
                if let Some(next) = operations::neighbor(world, window, direction) {
                    operations::focus(world, next);
                }
            }
            TileOperation::Move(direction) => {
                if let Some(next) = operations::neighbor(world, window, direction) {
                    operations::swap(world, window, next);
                }
            }
            TileOperation::Resize { axis, fraction } => {
                operations::resize(world, window, axis, fraction)
            }
        }
        changed |= layout::arrange(world, root, rect, gap)
            || world.resource::<FocusedWindow>().entity() != previous_focus;
    }
    if changed {
        world
            .resource_mut::<Messages<RequestRedraw>>()
            .write(RequestRedraw);
    }
}

fn prune(world: &mut World, container: Entity, manager: Entity) -> bool {
    let Some(children) = world
        .get::<TileContainer>(container)
        .map(|node| node.children.clone())
    else {
        return false;
    };
    let mut removed = false;
    for child in children {
        if world.get::<TileContainer>(child.entity).is_some() {
            removed |= prune(world, child.entity, manager);
        }
    }
    let Some(children) = world
        .get::<TileContainer>(container)
        .map(|node| node.children.clone())
    else {
        return removed;
    };
    let live: Vec<_> = children
        .iter()
        .filter(|child| {
            world.get::<TileContainer>(child.entity).is_some()
                || (world.get::<ManagedWindow>(child.entity).is_some()
                    && world
                        .get::<ManagedBy>(child.entity)
                        .is_some_and(|owner| owner.0 == manager))
        })
        .map(|child| child.entity)
        .collect();
    removed |= live.len() != children.len();
    for child in children
        .iter()
        .filter(|child| !live.contains(&child.entity))
    {
        if let Ok(mut entity) = world.get_entity_mut(child.entity) {
            entity.remove::<(TileParent, layout::LayoutRect)>();
        }
    }
    if removed && let Some(mut node) = world.get_mut::<TileContainer>(container) {
        node.children.retain(|child| live.contains(&child.entity));
    }
    // Explicit unary splits are pending insertion points and must survive idle
    // frames. Collapse only containers affected by removal, not every unary node.
    if removed {
        operations::compact(world, container);
    }
    removed
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
