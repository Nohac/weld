//! Split-layout adaptations of i3 124-move and 274-move-branch-position.

use std::collections::HashSet;

use bevy::ecs::query::With;
use weld_i3_quirks::I3MoveRequest;
use weld_tile::TileWorkspace;

use super::*;

fn move_window(app: &mut App, direction: Direction) {
    app.world_mut().trigger(I3MoveRequest(direction));
    app.world_mut().flush();
}

fn describe(app: &App, node: Entity, seen: &mut HashSet<Entity>) -> String {
    assert!(seen.insert(node), "duplicate or cyclic node");
    if let Some(window) = app.world().get::<ManagedWindow>(node) {
        return window.id.raw().to_string();
    }
    let container = app.world().get::<TileContainer>(node).expect("container");
    let children = container
        .children()
        .map(|(child, weight)| {
            assert_eq!(
                app.world()
                    .get::<TileParent>(child)
                    .expect("parent")
                    .entity(),
                node
            );
            assert!(weight.is_finite() && weight > 0.0);
            describe(app, child, seen)
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{}[{children}]",
        if container.axis() == SplitAxis::Horizontal {
            "H"
        } else {
            "V"
        }
    )
}

fn assert_tree(app: &mut App, expected: &str) {
    let root = app
        .world_mut()
        .query_filtered::<Entity, With<TileWorkspace>>()
        .single(app.world())
        .expect("workspace");
    let mut seen = HashSet::new();
    assert_eq!(describe(app, root, &mut seen), expected);
    let nodes = app
        .world_mut()
        .query::<&TileContainer>()
        .iter(app.world())
        .count()
        + app
            .world_mut()
            .query::<&ManagedWindow>()
            .iter(app.world())
            .count();
    assert_eq!(
        seen.len(),
        nodes,
        "every node must be reachable exactly once"
    );
    for node in app.world().resource::<TileFocusHistory>().recent() {
        assert!(seen.contains(&node), "retired node remains in history");
    }
}

#[test]
fn solitary_window_and_workspace_edges_do_not_wrap() {
    let mut app = app();
    window(&mut app, 1);
    for direction in [
        Direction::Left,
        Direction::Right,
        Direction::Up,
        Direction::Down,
    ] {
        move_window(&mut app, direction);
        assert_tree(&mut app, "H[1]");
    }
    let second = window(&mut app, 2);
    window(&mut app, 3);
    focus(&mut app, second);
    for (direction, expected) in [
        (Direction::Left, "H[2 1 3]"),
        (Direction::Left, "H[2 1 3]"),
        (Direction::Right, "H[1 2 3]"),
        (Direction::Right, "H[1 3 2]"),
        (Direction::Right, "H[1 3 2]"),
    ] {
        move_window(&mut app, direction);
        assert_tree(&mut app, expected);
        assert_eq!(selected(&app), second);
    }
}

#[test]
fn enter_reorder_extract_and_reenter_an_explicit_split() {
    let mut app = app();
    window(&mut app, 1);
    let second = window(&mut app, 2);
    let third = window(&mut app, 3);
    focus(&mut app, second);
    split(&mut app, SplitAxis::Vertical);
    focus(&mut app, third);
    assert_tree(&mut app, "H[1 V[2] 3]");
    for (direction, expected) in [
        (Direction::Left, "H[1 V[2 3]]"),
        (Direction::Up, "H[1 V[3 2]]"),
        (Direction::Left, "H[1 3 V[2]]"),
        (Direction::Right, "H[1 V[2 3]]"),
    ] {
        move_window(&mut app, direction);
        assert_tree(&mut app, expected);
        assert_eq!(selected(&app), third);
    }
}

#[test]
fn empty_source_split_is_removed_when_its_last_child_moves() {
    let mut app = app();
    let first = window(&mut app, 1);
    window(&mut app, 2);
    split(&mut app, SplitAxis::Vertical);
    window(&mut app, 3);
    focus(&mut app, first);
    split(&mut app, SplitAxis::Vertical);
    window(&mut app, 4);
    let source = app
        .world()
        .get::<TileParent>(first)
        .expect("source")
        .entity();
    assert_tree(&mut app, "H[V[1 4] V[2 3]]");
    move_window(&mut app, Direction::Right);
    assert_tree(&mut app, "H[V[1] V[2 3 4]]");
    focus(&mut app, first);
    move_window(&mut app, Direction::Right);
    assert_tree(&mut app, "H[V[2 3 4 1]]");
    assert!(app.world().get_entity(source).is_err());
}

#[test]
fn edge_of_nested_matching_split_extracts_before_crossing_a_leaf_neighbor() {
    let mut app = app();
    let first = window(&mut app, 1);
    window(&mut app, 2);
    focus(&mut app, first);
    split(&mut app, SplitAxis::Horizontal);
    window(&mut app, 3);
    assert_tree(&mut app, "H[H[1 3] 2]");
    move_window(&mut app, Direction::Right);
    assert_tree(&mut app, "H[H[1] 3 2]");
    move_window(&mut app, Direction::Right);
    assert_tree(&mut app, "H[H[1] 2 3]");
}

#[test]
fn singleton_branch_at_workspace_edge_is_kept_without_an_adjacent_output() {
    for (id, direction, expected) in [
        (1, Direction::Left, "H[V[1] 2]"),
        (2, Direction::Right, "H[1 V[2]]"),
    ] {
        let mut app = app();
        let first = window(&mut app, 1);
        let second = window(&mut app, 2);
        focus(&mut app, if id == 1 { first } else { second });
        split(&mut app, SplitAxis::Vertical);
        move_window(&mut app, direction);
        assert_tree(&mut app, expected);
    }
}

#[test]
fn perpendicular_workspace_move_preserves_the_remaining_group() {
    for (root_axis, direction, expected) in [
        (SplitAxis::Horizontal, Direction::Down, "V[H[1 3] 2]"),
        (SplitAxis::Horizontal, Direction::Up, "V[2 H[1 3]]"),
        (SplitAxis::Vertical, Direction::Right, "H[V[1 3] 2]"),
        (SplitAxis::Vertical, Direction::Left, "H[2 V[1 3]]"),
    ] {
        let mut app = app();
        window(&mut app, 1);
        split(&mut app, root_axis);
        let second = window(&mut app, 2);
        window(&mut app, 3);
        focus(&mut app, second);
        move_window(&mut app, direction);
        assert_tree(&mut app, expected);
        assert_eq!(selected(&app), second);
    }
}

#[test]
fn cross_axis_moves_flatten_redundant_alternating_wrappers() {
    let mut app = app();
    window(&mut app, 1);
    let second = window(&mut app, 2);
    move_window(&mut app, Direction::Down);
    assert_tree(&mut app, "V[H[1] 2]");
    move_window(&mut app, Direction::Left);
    assert_tree(&mut app, "H[2 1]");
    assert_eq!(selected(&app), second);
}

#[test]
fn branch_entry_uses_near_edge_or_remembered_child_for_every_direction() {
    for direction in [
        Direction::Left,
        Direction::Right,
        Direction::Up,
        Direction::Down,
    ] {
        let forward = matches!(direction, Direction::Right | Direction::Down);
        let axis = if matches!(direction, Direction::Left | Direction::Right) {
            SplitAxis::Horizontal
        } else {
            SplitAxis::Vertical
        };
        let cross = if axis == SplitAxis::Horizontal {
            SplitAxis::Vertical
        } else {
            SplitAxis::Horizontal
        };
        for target_axis in [axis, cross] {
            for nested_source in [false, true] {
                let mut app = app();
                let first = window(&mut app, 1);
                split(&mut app, axis);
                let second = window(&mut app, 2);
                let (moving, target) = if forward {
                    (first, second)
                } else {
                    (second, first)
                };
                focus(&mut app, target);
                split(&mut app, target_axis);
                let middle = window(&mut app, 3);
                let last = window(&mut app, 4);
                let target_parent = app
                    .world()
                    .get::<TileParent>(target)
                    .expect("target parent")
                    .entity();
                focus(&mut app, middle);
                focus(&mut app, moving);
                if nested_source {
                    split(&mut app, cross);
                    window(&mut app, 5);
                    focus(&mut app, moving);
                }
                move_window(&mut app, direction);
                let actual: Vec<_> = app
                    .world()
                    .get::<TileContainer>(target_parent)
                    .expect("destination")
                    .children()
                    .map(|(child, _)| child)
                    .collect();
                let expected = if target_axis != axis {
                    vec![target, middle, moving, last]
                } else if forward {
                    vec![moving, target, middle, last]
                } else {
                    vec![target, middle, last, moving]
                };
                assert_eq!(
                    actual, expected,
                    "{direction:?} {target_axis:?} nested={nested_source}"
                );
                assert_eq!(selected(&app), moving);
            }
        }
    }
}

#[test]
fn branch_entry_recurses_through_mixed_axes() {
    let mut app = app();
    let moving = window(&mut app, 1);
    window(&mut app, 2);
    split(&mut app, SplitAxis::Vertical);
    window(&mut app, 3);
    split(&mut app, SplitAxis::Horizontal);
    window(&mut app, 4);
    focus(&mut app, moving);
    move_window(&mut app, Direction::Right);
    assert_tree(&mut app, "H[V[2 H[1 3 4]]]");
}

#[test]
fn a_batch_observes_root_wrapping_and_reparenting_before_the_next_move() {
    let mut app = app();
    window(&mut app, 1);
    let second = window(&mut app, 2);
    window(&mut app, 3);
    focus(&mut app, second);
    app.world_mut()
        .run_system_once(|mut commands: Commands| {
            commands.trigger(I3MoveRequest(Direction::Down));
            commands.trigger(I3MoveRequest(Direction::Up));
        })
        .expect("batch");
    assert_tree(&mut app, "V[H[1 3 2]]");
    assert_eq!(selected(&app), second);
}

#[test]
fn resized_share_follows_sibling_and_new_parent_assigns_mean_share() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    focus(&mut app, first);
    app.world_mut()
        .trigger(TileRequest::Focused(TileOperation::Resize {
            axis: SplitAxis::Horizontal,
            fraction: 0.2,
        }));
    app.world_mut().flush();
    let size = app
        .world()
        .get::<WindowGeometry>(first)
        .expect("geometry")
        .size;
    move_window(&mut app, Direction::Right);
    assert_tree(&mut app, "H[2 1]");
    assert_eq!(
        app.world()
            .get::<WindowGeometry>(first)
            .expect("geometry")
            .size,
        size
    );
    focus(&mut app, second);
    split(&mut app, SplitAxis::Vertical);
    window(&mut app, 3);
    focus(&mut app, first);
    move_window(&mut app, Direction::Left);
    assert_tree(&mut app, "H[V[2 3 1]]");
    for window in [first, second] {
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("geometry")
                .size,
            Vec2::new(800.0, 200.0)
        );
    }
}

#[test]
fn depth_limited_workspace_wrap_stops_without_mutating_or_retrying_forever() {
    let mut app = app();
    let outside = window(&mut app, 1);
    window(&mut app, 2);
    for id in 3..=65 {
        split(&mut app, SplitAxis::Vertical);
        window(&mut app, id);
    }
    let root = app.world().get::<ManagedBy>(outside).expect("root").0;
    let before = describe(&app, root, &mut HashSet::new());
    focus(&mut app, outside);
    move_window(&mut app, Direction::Up);
    assert_tree(&mut app, &before);
    assert_eq!(selected(&app), outside);
}

#[test]
fn move_and_focus_share_startup_order_and_retained_slots_keep_identity() {
    let mut app = App::new();
    app.init_resource::<SurfaceActionQueue>().add_plugins((
        WindowPlugin,
        TilePlugin,
        I3QuirksPlugin,
    ));
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    move_window(&mut app, Direction::Left);
    navigate(&mut app, Direction::Right);
    output(&mut app);
    app.update();
    assert_tree(&mut app, "H[2 1]");
    assert_eq!(selected(&app), first);
    assert_eq!(
        app.world().get::<WindowVacancy>(second),
        Some(&WindowVacancy::Retain)
    );
}

#[test]
fn close_immediately_after_reparent_uses_the_new_branch_history() {
    let mut app = app();
    let moving = window(&mut app, 1);
    window(&mut app, 2);
    split(&mut app, SplitAxis::Vertical);
    let remembered = window(&mut app, 3);
    focus(&mut app, moving);
    move_window(&mut app, Direction::Right);
    app.world_mut().despawn(moving);
    app.update();
    assert_eq!(selected(&app), remembered);
    assert_tree(&mut app, "H[V[2 3]]");
}
