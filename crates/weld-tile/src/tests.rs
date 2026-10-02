use bevy::{
    app::{App, PreUpdate},
    ecs::{
        change_detection::DetectChanges,
        message::Messages,
        observer::On,
        query::{Changed, With},
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Commands, Query, Res, ResMut, RunSystemOnce},
    },
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
    WindowCommand, WindowCommandKind, WindowGeometry, WindowId, WindowPlugin,
    WindowPresentationOverride, WindowVacancy,
};

use super::*;

#[test]
fn panel_work_area_reflows_tiles_without_changing_output_geometry() {
    let mut app = app();
    let window = window(&mut app, 1);
    let output = app
        .world_mut()
        .query_filtered::<Entity, With<WeldOutput>>()
        .single(app.world())
        .expect("output");
    let original = *app.world().get::<OutputGeometry>(output).expect("geometry");
    app.world_mut()
        .entity_mut(output)
        .insert(weld_app::output::OutputWorkArea {
            position: Vec2::new(0.0, 32.0),
            size: Vec2::new(800.0, 568.0),
        });
    app.update();
    assert_eq!(geometry(&app, window).position, Vec2::new(0.0, 32.0));
    assert_eq!(geometry(&app, window).size, Vec2::new(800.0, 568.0));
    assert_eq!(app.world().get::<OutputGeometry>(output), Some(&original));
    app.world_mut()
        .entity_mut(output)
        .remove::<weld_app::output::OutputWorkArea>();
    app.update();
    assert_eq!(geometry(&app, window).position, Vec2::ZERO);
    assert_eq!(geometry(&app, window).size, Vec2::new(800.0, 600.0));
}

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
    let output = app
        .world_mut()
        .spawn((
            WeldOutput {
                id: OutputId::new(1),
            },
            PrimaryOutput,
            OutputGeometry::from_physical(UVec2::new(800, 600), 1.0),
        ))
        .id();
    create_workspace(&mut app, output);
    app
}

fn create_workspace(app: &mut App, output: bevy::ecs::entity::Entity) {
    app.world_mut()
        .run_system_once(
            move |mut creation: weld_window::workspace::WorkspaceCreation,
                  mut commands: Commands| {
                let workspace = creation.create("1".to_owned(), output).expect("workspace");
                commands.trigger(weld_window::workspace::WorkspaceRequest::SetVisible {
                    workspace,
                    visible: true,
                });
                commands.trigger(weld_window::workspace::WorkspaceRequest::Focus(workspace));
            },
        )
        .expect("workspace fixture");
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

#[test]
fn structural_edits_publish_before_the_next_batched_operation() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    {
        let mut commands = app.world_mut().resource_mut::<TileCommands>();
        for operation in [
            TileOperation::Split(SplitAxis::Vertical),
            TileOperation::Focus(Direction::Left),
            TileOperation::Move(Direction::Right),
            TileOperation::Focus(Direction::Left),
        ] {
            commands.push_focused(operation).expect("capacity");
        }
    }
    app.update();
    assert_eq!(geometry(&app, first).position.x, 400.0);
    assert_eq!(geometry(&app, second).position.x, 0.0);
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(second)
    );
    let parent = app
        .world()
        .get::<TileParent>(first)
        .expect("new split")
        .entity();
    assert_eq!(
        app.world()
            .get::<TileContainer>(parent)
            .expect("published container")
            .axis(),
        SplitAxis::Vertical
    );
}

#[derive(Resource, Default)]
struct Changes {
    layouts: usize,
    geometries: usize,
    containers: usize,
    history: usize,
}

#[test]
fn unchanged_frames_do_not_relayout_or_republish_components() {
    let mut app = app();
    app.init_resource::<Changes>()
        .add_observer(
            |_: On<layout::LayoutRequested>, mut changes: ResMut<Changes>| changes.layouts += 1,
        )
        .add_systems(
            PreUpdate,
            (|windows: Query<(), (With<TileParent>, Changed<WindowGeometry>)>,
              containers: Query<(), Changed<TileContainer>>,
              history: Res<TileFocusHistory>,
              mut changes: ResMut<Changes>| {
                changes.geometries += windows.iter().count();
                changes.containers += containers.iter().count();
                changes.history += usize::from(history.is_changed());
            })
            .after(TileSystems::Layout),
        );
    window(&mut app, 1);
    window(&mut app, 2);
    *app.world_mut().resource_mut::<Changes>() = Changes::default();
    for _ in 0..10 {
        app.update();
    }
    let changes = app.world().resource::<Changes>();
    assert_eq!(
        (
            changes.layouts,
            changes.geometries,
            changes.containers,
            changes.history
        ),
        (0, 0, 0, 0)
    );
}

#[test]
fn commands_wait_for_the_first_output_and_admission() {
    let mut app = App::new();
    app.init_resource::<SurfaceActionQueue>()
        .add_plugins((WindowPlugin, TilePlugin));
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    app.world_mut()
        .trigger(TileRequest::Focused(TileOperation::Focus(Direction::Left)));
    app.update();
    assert!(app.world().get::<TileParent>(first).is_none());
    assert_eq!(app.world().resource::<TileCommands>().len(), 1);
    let output = app
        .world_mut()
        .spawn((
            WeldOutput {
                id: OutputId::new(1),
            },
            PrimaryOutput,
            OutputGeometry::from_physical(UVec2::new(800, 600), 1.0),
        ))
        .id();
    create_workspace(&mut app, output);
    app.update();
    assert!(app.world().get::<TileParent>(second).is_some());
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(first)
    );
    assert!(app.world().resource::<TileCommands>().is_empty());
}

#[test]
fn native_focus_commands_request_redraw() {
    let mut app = app();
    let first = window(&mut app, 1);
    window(&mut app, 2);
    app.world_mut()
        .resource_mut::<Messages<bevy::window::RequestRedraw>>()
        .clear();
    command(&mut app, 2, TileOperation::Focus(Direction::Left));
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(first)
    );
    assert!(
        !app.world()
            .resource::<Messages<bevy::window::RequestRedraw>>()
            .is_empty()
    );
}

#[test]
fn ownership_transfer_compacts_multiple_levels_before_admission() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    let third = window(&mut app, 3);
    command(&mut app, 3, TileOperation::Split(SplitAxis::Horizontal));
    let fourth = window(&mut app, 4);
    let other_manager = app.world_mut().spawn_empty().id();
    for entity in [second, third] {
        app.world_mut()
            .entity_mut(entity)
            .insert(ManagedBy(other_manager));
    }
    app.update();
    assert_eq!(
        app.world().get::<TileParent>(first),
        app.world().get::<TileParent>(fourth)
    );
    assert!(app.world().get::<TileParent>(second).is_none());
    assert!(app.world().get::<TileParent>(third).is_none());
    assert_eq!(geometry(&app, fourth).size, Vec2::new(400.0, 600.0));
    assert_eq!(
        app.world_mut()
            .query::<&TileContainer>()
            .iter(app.world())
            .count(),
        1
    );
}

#[test]
fn structural_place_lifts_a_child_to_its_grandparent_without_changing_selection() {
    // Topology cases from i3 306-move-to-parent.t, exercised through the native
    // edit primitive before marks and criteria become config features.
    for nested in [false, true] {
        let mut app = app();
        let first = window(&mut app, 1);
        if nested {
            window(&mut app, 9);
            command(&mut app, 1, TileOperation::Split(SplitAxis::Vertical));
            app.world_mut().trigger(WindowCommand {
                window: first,
                kind: WindowCommandKind::Focus,
            });
            app.world_mut().flush();
        }
        let second = window(&mut app, 2);
        let third = window(&mut app, 3);
        command(&mut app, 2, TileOperation::Split(SplitAxis::Horizontal));
        let wrapper = app
            .world()
            .get::<TileParent>(second)
            .expect("parent")
            .entity();
        let destination = app
            .world()
            .get::<TileParent>(wrapper)
            .expect("grandparent")
            .entity();
        app.world_mut().trigger(WindowCommand {
            window: second,
            kind: WindowCommandKind::Focus,
        });
        app.world_mut().trigger(TileTreeEdit::Place {
            node: second,
            anchor: wrapper,
            side: TileSide::After,
        });
        app.world_mut().flush();
        assert_eq!(
            app.world()
                .get::<TileContainer>(destination)
                .expect("destination")
                .children()
                .map(|(node, _)| node)
                .collect::<Vec<_>>(),
            [first, second, third]
        );
        assert!(app.world().get_entity(wrapper).is_err());
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(second)
        );
    }
}

#[test]
fn structural_edits_reject_cycles_stale_nodes_and_foreign_ownership_atomically() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    let third = window(&mut app, 3);
    let root = app.world().get::<ManagedBy>(first).expect("owner").0;
    let branch = app
        .world()
        .get::<TileParent>(second)
        .expect("parent")
        .entity();
    let stale = app.world_mut().spawn_empty().id();
    app.world_mut().despawn(stale);
    for edit in [
        TileTreeEdit::Place {
            node: branch,
            anchor: second,
            side: TileSide::After,
        },
        TileTreeEdit::Place {
            node: second,
            anchor: second,
            side: TileSide::Before,
        },
        TileTreeEdit::Place {
            node: root,
            anchor: third,
            side: TileSide::After,
        },
        TileTreeEdit::Place {
            node: second,
            anchor: stale,
            side: TileSide::After,
        },
        TileTreeEdit::Place {
            node: stale,
            anchor: second,
            side: TileSide::After,
        },
    ] {
        app.world_mut().trigger(edit);
        app.world_mut().flush();
        assert_eq!(
            app.world()
                .get::<TileContainer>(root)
                .expect("root")
                .children()
                .map(|(node, _)| node)
                .collect::<Vec<_>>(),
            [first, branch]
        );
        assert_eq!(
            app.world()
                .get::<TileContainer>(branch)
                .expect("branch")
                .children()
                .map(|(node, _)| node)
                .collect::<Vec<_>>(),
            [second, third]
        );
    }
    let foreign = app.world_mut().spawn_empty().id();
    app.world_mut()
        .entity_mut(second)
        .insert(ManagedBy(foreign));
    for edit in [
        TileTreeEdit::Place {
            node: second,
            anchor: first,
            side: TileSide::Before,
        },
        TileTreeEdit::Place {
            node: first,
            anchor: second,
            side: TileSide::Before,
        },
        TileTreeEdit::WrapChildren {
            container: root,
            axis: SplitAxis::Vertical,
        },
    ] {
        app.world_mut().trigger(edit);
        app.world_mut().flush();
    }
    assert_eq!(
        app.world().get::<TileContainer>(root).expect("root").axis(),
        SplitAxis::Horizontal
    );
    assert_eq!(
        app.world()
            .get::<TileParent>(second)
            .expect("parent")
            .entity(),
        branch
    );
    assert_eq!(
        app.world()
            .get::<TileParent>(first)
            .expect("parent")
            .entity(),
        root
    );
}

#[test]
fn structural_edits_can_reparent_a_whole_subtree() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 2, TileOperation::Split(SplitAxis::Vertical));
    let third = window(&mut app, 3);
    let branch = app
        .world()
        .get::<TileParent>(second)
        .expect("branch")
        .entity();
    app.world_mut().trigger(TileTreeEdit::Place {
        node: branch,
        anchor: first,
        side: TileSide::Before,
    });
    app.world_mut().flush();
    assert_eq!(
        app.world()
            .get::<TileParent>(third)
            .expect("unchanged branch")
            .entity(),
        branch
    );
    assert_eq!(geometry(&app, first).position.x, 400.0);
    assert_eq!(geometry(&app, second).position, Vec2::ZERO);
}

#[test]
fn flattening_unary_groups_preserves_proportions_and_history() {
    for same_axis in [false, true] {
        let mut app = app();
        let first = window(&mut app, 1);
        let second = window(&mut app, 2);
        let root = app.world().get::<ManagedBy>(first).expect("root").0;
        app.world_mut().trigger(TileTreeEdit::WrapChildren {
            container: root,
            axis: SplitAxis::Vertical,
        });
        app.world_mut().flush();
        app.world_mut().trigger(TileTreeEdit::WrapChildren {
            container: root,
            axis: SplitAxis::Horizontal,
        });
        app.world_mut().flush();
        let inner = app
            .world()
            .get::<TileParent>(first)
            .expect("inner")
            .entity();
        let outer = app
            .world()
            .get::<TileParent>(inner)
            .expect("outer")
            .entity();
        if same_axis {
            app.world_mut()
                .get_mut::<TileContainer>(outer)
                .expect("outer")
                .axis = SplitAxis::Horizontal;
        }
        command(
            &mut app,
            1,
            TileOperation::Resize {
                axis: SplitAxis::Horizontal,
                fraction: 0.2,
            },
        );
        let before = (geometry(&app, first), geometry(&app, second));
        app.world_mut()
            .trigger(TileTreeEdit::Flatten { container: outer });
        app.world_mut().flush();
        assert_eq!((geometry(&app, first), geometry(&app, second)), before);
        assert_eq!(
            app.world()
                .get::<TileParent>(first)
                .expect("promoted")
                .entity(),
            root
        );
        assert!(app.world().get_entity(inner).is_err());
        assert!(app.world().get_entity(outer).is_err());
        assert_eq!(
            app.world()
                .resource::<TileFocusHistory>()
                .recent()
                .filter(|node| *node != root)
                .collect::<Vec<_>>(),
            [second, first]
        );
    }
}

#[test]
fn reparent_rejects_a_subtree_whose_height_exceeds_the_destination_budget() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    command(&mut app, 1, TileOperation::Split(SplitAxis::Horizontal));
    app.world_mut().trigger(WindowCommand {
        window: first,
        kind: WindowCommandKind::Focus,
    });
    app.world_mut().flush();
    window(&mut app, 3);
    let source = app
        .world()
        .get::<TileParent>(first)
        .expect("source")
        .entity();
    let source_parent = app
        .world()
        .get::<TileParent>(source)
        .expect("source parent")
        .entity();
    app.world_mut().trigger(WindowCommand {
        window: second,
        kind: WindowCommandKind::Focus,
    });
    app.world_mut().flush();
    let mut last = second;
    for id in 4..=66 {
        command(
            &mut app,
            if id == 4 { 2 } else { id - 1 },
            TileOperation::Split(SplitAxis::Vertical),
        );
        last = window(&mut app, id);
    }
    let destination = app
        .world()
        .get::<TileParent>(last)
        .expect("destination")
        .entity();
    app.world_mut().trigger(TileTreeEdit::Place {
        node: source,
        anchor: last,
        side: TileSide::Before,
    });
    app.world_mut().flush();
    assert_eq!(
        app.world()
            .get::<TileParent>(source)
            .expect("source parent")
            .entity(),
        source_parent
    );
    assert_eq!(
        app.world()
            .get::<TileParent>(last)
            .expect("destination")
            .entity(),
        destination
    );
    assert_eq!(
        app.world().get::<TileParent>(first).expect("leaf").entity(),
        source
    );
}
