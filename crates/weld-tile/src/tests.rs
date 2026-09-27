use bevy::{
    app::App,
    math::{UVec2, Vec2},
};
use weld_app::{
    output::{OutputGeometry, OutputId, PrimaryOutput, WeldOutput},
    surface::{
        ClientProvenance, ClientSource, ClientToplevel, MappedSurface, SurfaceAction,
        SurfaceActionQueue, SurfaceId, take_surface_actions,
    },
};
use weld_window::{
    FocusedWindow, ManagedBy, ManagedWindow, OccupiesWindow, PresentationInsets, PresentsWindow,
    WindowGeometry, WindowId, WindowPlugin, WindowPresentationOverride, WindowVacancy,
};

use super::*;

#[test]
fn manager_loss_readmits_once_and_transfer_relinquishes_the_leaf() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    app.world_mut().entity_mut(second).remove::<ManagedBy>();
    app.update();
    let count = app
        .world_mut()
        .query::<&TileContainer>()
        .iter(app.world())
        .flat_map(TileContainer::children)
        .filter(|(entity, _)| *entity == second)
        .count();
    assert_eq!(count, 1);
    assert_eq!(geometry(&app, first).size.x, 400.0);
    let other_manager = app.world_mut().spawn_empty().id();
    app.world_mut()
        .entity_mut(second)
        .insert(ManagedBy(other_manager));
    app.update();
    assert!(app.world().get::<TileParent>(second).is_none());
    assert_eq!(
        app.world().get::<ManagedBy>(second).expect("other owner").0,
        other_manager
    );
    assert_eq!(geometry(&app, first).size.x, 800.0);
}

#[test]
fn moving_between_parents_keeps_bidirectional_edges_and_slot_geometry() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    let third = window(&mut app, 3);
    let before_first = geometry(&app, first);
    let before_third = geometry(&app, third);
    command(&mut app, 3, TileOperation::Move(Direction::Left));
    assert_eq!(geometry(&app, first), before_third);
    assert_eq!(geometry(&app, third), before_first);
    for window in [first, second, third] {
        let parent = app
            .world()
            .get::<TileParent>(window)
            .expect("parent")
            .entity();
        assert_eq!(
            app.world()
                .get::<TileContainer>(parent)
                .expect("container")
                .children()
                .filter(|(entity, _)| *entity == window)
                .count(),
            1
        );
    }
}

#[test]
fn occupant_detach_and_reclaim_preserve_layout_and_use_shared_resize_effects() {
    let mut app = app();
    let surface = SurfaceId::for_test(7);
    let client = app
        .world_mut()
        .spawn((
            ClientSource {
                id: surface.source(),
                provenance: ClientProvenance::Local,
            },
            ClientToplevel { surface },
            MappedSurface {
                logical_size: Vec2::new(320.0, 240.0),
                alpha_mode: Default::default(),
                visual_offset: Vec2::ZERO,
                visual_size: Vec2::new(320.0, 240.0),
                opaque: true,
            },
        ))
        .id();
    app.update();
    let first = app
        .world()
        .get::<OccupiesWindow>(client)
        .expect("admitted")
        .0;
    let frame = app
        .world_mut()
        .spawn((
            PresentsWindow(first),
            PresentationInsets::new(2.0, 20.0, 2.0, 2.0),
        ))
        .id();
    app.update();
    assert!(
        take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
            surface,
            logical_size: UVec2::new(796, 578),
            resizing: false,
        })
    );
    let parent = *app.world().get::<TileParent>(first).expect("parent");
    // Mirror the shared hoist boundary: the managed source slot is retained,
    // the client no longer occupies it, and the hoist presenter owns its root.
    app.world_mut().entity_mut(first).insert((
        WindowVacancy::Retain,
        WindowPresentationOverride::new(frame),
    ));
    app.world_mut()
        .entity_mut(client)
        .remove::<(OccupiesWindow, MappedSurface)>();
    window(&mut app, 50);
    assert_eq!(app.world().get::<TileParent>(first), Some(&parent));
    assert_eq!(geometry(&app, first).size.x, 400.0);
    assert!(
        !take_surface_actions(app.world_mut())
            .iter()
            .any(|action| matches!(action, SurfaceAction::Resize { .. }))
    );
    app.world_mut().entity_mut(client).insert((
        OccupiesWindow(first),
        MappedSurface {
            logical_size: Vec2::new(320.0, 240.0),
            alpha_mode: Default::default(),
            visual_offset: Vec2::ZERO,
            visual_size: Vec2::new(320.0, 240.0),
            opaque: true,
        },
    ));
    app.world_mut()
        .entity_mut(first)
        .remove::<WindowPresentationOverride>();
    app.update();
    assert_eq!(
        app.world()
            .get::<OccupiesWindow>(client)
            .expect("reclaimed")
            .0,
        first
    );
    assert_eq!(app.world().get::<TileParent>(first), Some(&parent));
    assert!(
        take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
            surface,
            logical_size: UVec2::new(396, 578),
            resizing: false,
        })
    );
}

fn app() -> App {
    let mut app = App::new();
    app.init_resource::<SurfaceActionQueue>()
        .add_plugins((WindowPlugin, TilePlugin));
    app.insert_resource(TileSettings {
        inner_gap: 0,
        outer_gap: 0,
        ..Default::default()
    });
    app.world_mut().spawn((
        WeldOutput {
            id: OutputId::new(1),
        },
        PrimaryOutput,
        OutputGeometry::from_physical(UVec2::new(800, 600), 1.0),
    ));
    app
}

fn window(app: &mut App, id: u64) -> Entity {
    let entity = app
        .world_mut()
        .spawn((
            ManagedWindow {
                id: WindowId::new(id),
            },
            WindowVacancy::Retain,
        ))
        .id();
    app.update();
    entity
}

fn command(app: &mut App, id: u64, operation: TileOperation) {
    app.world_mut()
        .resource_mut::<TileCommands>()
        .push(TileCommand {
            window: WindowId::new(id),
            operation,
        })
        .expect("queue has capacity");
    app.update();
}

fn geometry(app: &App, window: Entity) -> WindowGeometry {
    *app.world()
        .get::<WindowGeometry>(window)
        .expect("managed window geometry")
}

#[test]
fn nested_splits_relayout_live_without_replacing_identity_or_focus() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    app.update(); // A pending unary split survives a frame without a new client.
    let third = window(&mut app, 3);
    assert_eq!(geometry(&app, first).size, Vec2::new(400.0, 600.0));
    assert_eq!(geometry(&app, second).size, Vec2::new(400.0, 300.0));
    assert_eq!(geometry(&app, third).position, Vec2::new(400.0, 300.0));
    let parent = *app.world().get::<TileParent>(third).expect("parent");
    let settings = TileSettings {
        inner_gap: 10,
        outer_gap: 20,
        default_axis: SplitAxis::Horizontal,
    };
    app.insert_resource(settings);
    app.update();
    assert_eq!(geometry(&app, first).position, Vec2::splat(20.0));
    assert_eq!(geometry(&app, second).size, Vec2::new(375.0, 275.0));
    assert_eq!(app.world().get::<TileParent>(third), Some(&parent));
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(third)
    );
}

#[test]
fn removal_compacts_nested_tree_and_vacancies_keep_their_slot() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    let third = window(&mut app, 3);
    let fourth = window(&mut app, 4);
    app.world_mut().despawn(second);
    app.world_mut().despawn(third);
    app.world_mut().despawn(fourth);
    app.update();
    assert_eq!(geometry(&app, first).size, Vec2::new(800.0, 600.0));
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(first)
    );
    app.update();
    assert!(app.world().get::<ManagedWindow>(first).is_some());
}

#[test]
fn directional_focus_move_and_resize_are_native_operations() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Focus(Direction::Left));
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(first)
    );
    command(&mut app, 1, TileOperation::Move(Direction::Right));
    assert_eq!(geometry(&app, first).position.x, 400.0);
    assert_eq!(geometry(&app, second).position.x, 0.0);
    command(
        &mut app,
        1,
        TileOperation::Resize {
            axis: SplitAxis::Horizontal,
            fraction: 0.1,
        },
    );
    assert!((geometry(&app, first).size.x - 480.0).abs() < 0.01);
    let before = geometry(&app, first);
    command(
        &mut app,
        1,
        TileOperation::Resize {
            axis: SplitAxis::Horizontal,
            fraction: f32::NAN,
        },
    );
    assert_eq!(geometry(&app, first), before);
}

#[test]
fn resizing_output_changes_geometry_not_tree_membership() {
    let mut app = app();
    let first = window(&mut app, 1);
    let parent = *app.world().get::<TileParent>(first).expect("parent");
    let output = app
        .world_mut()
        .query::<(Entity, &WeldOutput)>()
        .iter(app.world())
        .next()
        .expect("output")
        .0;
    app.world_mut()
        .entity_mut(output)
        .insert(OutputGeometry::from_physical(UVec2::new(1920, 1080), 2.0));
    app.update();
    assert_eq!(geometry(&app, first).size, Vec2::new(960.0, 540.0));
    assert_eq!(app.world().get::<TileParent>(first), Some(&parent));
}

#[test]
fn batched_navigation_resolves_focus_after_each_operation() {
    let mut app = app();
    let first = window(&mut app, 1);
    window(&mut app, 2);
    window(&mut app, 3);
    for _ in 0..2 {
        app.world_mut()
            .resource_mut::<TileCommands>()
            .push_focused(TileOperation::Focus(Direction::Left))
            .expect("capacity");
    }
    app.update();
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(first)
    );
}
