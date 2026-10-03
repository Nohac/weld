//! Behavioral scenarios adapted from i3's focus and movement tests at
//! 903bcd518df32b0e055b17f5da3f988a0187fd3d. See ../UPSTREAM.md and ../LICENSE-i3.

use bevy::{
    app::{App, PreUpdate},
    ecs::{
        entity::Entity,
        schedule::IntoScheduleConfigs,
        system::{Commands, RunSystemOnce},
    },
    math::{UVec2, Vec2},
};
use weld_app::{
    output::{OutputGeometry, OutputId, PrimaryOutput, WeldOutput},
    surface::{SurfaceAction, SurfaceActionQueue, take_surface_actions},
};
use weld_float::{FloatBehaviorPlugin, FloatManagement};
use weld_i3_quirks::{
    FocusWrapping, I3FocusModeToggle, I3FocusRequest, I3QuirksPlugin,
    workspace::{I3WorkspaceRequest, WorkspaceTarget},
};
use weld_tile::{
    Direction, SplitAxis, TileCommand, TileContainer, TileFloatingRequest, TileFocusHistory,
    TileOperation, TileParent, TilePlugin, TileRequest, TileSettings, TileSystems, TileWorkspace,
};
use weld_window::{
    FloatingWindow, FocusedWindow, ManagedBy, ManagedWindow, WindowCommand, WindowCommandKind,
    WindowGeometry, WindowId, WindowIntent, WindowIntentKind, WindowInteractionKind,
    WindowInteractionSession, WindowPlugin, WindowVacancy, WindowZOrder,
};

#[path = "cases/movement.rs"]
mod movement;
#[path = "cases/workspace.rs"]
mod workspace;

#[test]
fn floating_plane_focus_and_workspace_roundtrip_preserve_selection() {
    let mut app = app();
    let tiled = window(&mut app, 1);
    let floating = window(&mut app, 2);
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(floating),
        enabled: Some(true),
    });
    app.update();
    app.world_mut().trigger(I3FocusModeToggle);
    app.update();
    assert_eq!(selected(&app), tiled);
    app.world_mut().trigger(I3FocusModeToggle);
    app.update();
    assert_eq!(selected(&app), floating);
    app.world_mut()
        .trigger(I3WorkspaceRequest::Switch(WorkspaceTarget::Name(
            "2".into(),
        )));
    app.update();
    app.world_mut()
        .trigger(I3WorkspaceRequest::Switch(WorkspaceTarget::Name(
            "1".into(),
        )));
    app.update();
    assert_eq!(selected(&app), floating);
    app.world_mut().entity_mut(floating).despawn();
    app.update();
    assert_eq!(selected(&app), tiled);
}

#[test]
fn floating_window_transfer_updates_workspace_without_adding_a_tile() {
    let mut app = app();
    let tiled = window(&mut app, 1);
    let floating = window(&mut app, 2);
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(floating),
        enabled: Some(true),
    });
    app.update();
    app.world_mut()
        .trigger(I3WorkspaceRequest::MoveWindow(WorkspaceTarget::Name(
            "2".into(),
        )));
    app.update();
    assert_eq!(selected(&app), tiled);
    assert_eq!(
        app.world().get::<weld_window::WindowVisibility>(floating),
        Some(&weld_window::WindowVisibility::Hidden)
    );
    app.world_mut()
        .trigger(I3WorkspaceRequest::Switch(WorkspaceTarget::Name(
            "2".into(),
        )));
    app.update();
    assert_eq!(selected(&app), floating);
    assert!(app.world().get::<TileParent>(floating).is_none());
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(floating),
        enabled: Some(false),
    });
    app.update();
    assert!(app.world().get::<TileParent>(floating).is_some());
}

#[test]
fn mixed_assembly_raises_floating_windows_and_ends_drag_when_retiled() {
    let mut app = app();
    app.add_message::<weld_app::surface::ToplevelInteractionRequest>()
        .add_plugins(FloatBehaviorPlugin)
        .configure_sets(
            PreUpdate,
            FloatManagement
                .after(TileSystems::Prepare)
                .before(TileSystems::Actions),
        );
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    for window in [first, second] {
        app.world_mut().trigger(TileFloatingRequest {
            window: Some(window),
            enabled: Some(true),
        });
        app.update();
    }
    let order = |app: &App, window| app.world().get::<WindowZOrder>(window).expect("z").0;
    assert!(order(&app, first) > 0);
    assert!(order(&app, second) > order(&app, first));
    app.world_mut().trigger(WindowIntent {
        window: first,
        kind: WindowIntentKind::Activate,
    });
    app.update();
    assert!(order(&app, first) > order(&app, second));
    app.world_mut().trigger(WindowCommand {
        window: first,
        kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Move),
    });
    app.world_mut().flush();
    assert!(app.world().get::<WindowInteractionSession>(first).is_some());
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(first),
        enabled: Some(false),
    });
    app.update();
    assert!(app.world().get::<WindowInteractionSession>(first).is_none());
    assert!(app.world().get::<FloatingWindow>(first).is_none());
    assert_eq!(order(&app, first), 0);
}

#[test]
fn removing_tile_workspace_releases_both_layout_planes_and_readmits_floating_state() {
    let mut app = app();
    let tiled = window(&mut app, 1);
    let floating = window(&mut app, 2);
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(floating),
        enabled: Some(true),
    });
    app.update();
    let workspace = app.world().get::<ManagedBy>(tiled).expect("owner").0;
    let previous = *app
        .world()
        .get::<WindowGeometry>(floating)
        .expect("geometry");
    app.world_mut()
        .entity_mut(workspace)
        .remove::<TileWorkspace>();
    app.world_mut().flush();
    assert!(app.world().get::<ManagedBy>(tiled).is_none());
    assert!(app.world().get::<ManagedBy>(floating).is_none());
    app.update();
    assert_eq!(
        app.world().get::<ManagedBy>(floating),
        Some(&ManagedBy(workspace))
    );
    assert!(app.world().get::<FloatingWindow>(floating).is_some());
    assert!(app.world().get::<TileParent>(floating).is_none());
    assert_eq!(
        *app.world()
            .get::<WindowGeometry>(floating)
            .expect("geometry"),
        previous
    );
}

fn app() -> App {
    let mut app = App::new();
    app.init_resource::<SurfaceActionQueue>().add_plugins((
        WindowPlugin,
        TilePlugin,
        I3QuirksPlugin,
    ));
    app.insert_resource(TileSettings {
        inner_gap: 0,
        outer_gap: 0,
        ..Default::default()
    });
    output(&mut app);
    app
}

fn output(app: &mut App) {
    app.world_mut().spawn((
        WeldOutput {
            id: OutputId::new(1),
        },
        PrimaryOutput,
        OutputGeometry::from_physical(UVec2::new(800, 600), 1.0),
    ));
}

fn window(app: &mut App, id: u64) -> Entity {
    let window = app
        .world_mut()
        .spawn((
            ManagedWindow {
                id: WindowId::new(id),
            },
            WindowVacancy::Retain,
        ))
        .id();
    app.update();
    window
}

fn focus(app: &mut App, window: Entity) {
    app.world_mut().trigger(WindowCommand {
        window,
        kind: WindowCommandKind::Focus,
    });
    app.world_mut().flush();
}

fn split(app: &mut App, axis: SplitAxis) {
    app.world_mut()
        .trigger(TileRequest::Focused(TileOperation::Split(axis)));
    app.update();
}

fn navigate(app: &mut App, direction: Direction) {
    app.world_mut().trigger(I3FocusRequest(direction));
    app.world_mut().flush();
}

fn selected(app: &App) -> Entity {
    app.world()
        .resource::<FocusedWindow>()
        .entity()
        .expect("focused window")
}

#[test]
fn single_window_navigation_is_a_noop() {
    let mut app = app();
    let only = window(&mut app, 1);
    for mode in [
        FocusWrapping::No,
        FocusWrapping::Yes,
        FocusWrapping::Force,
        FocusWrapping::Workspace,
    ] {
        app.insert_resource(mode);
        for direction in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            navigate(&mut app, direction);
            assert_eq!(selected(&app), only);
        }
    }
}

#[test]
fn siblings_navigate_and_wrap_in_layout_order() {
    let mut app = app();
    let left = window(&mut app, 1);
    let middle = window(&mut app, 2);
    let right = window(&mut app, 3);
    for expected in [left, middle, right] {
        navigate(&mut app, Direction::Right);
        assert_eq!(selected(&app), expected);
    }
    for expected in [middle, left, right] {
        navigate(&mut app, Direction::Left);
        assert_eq!(selected(&app), expected);
    }
    app.insert_resource(FocusWrapping::No);
    navigate(&mut app, Direction::Right);
    assert_eq!(selected(&app), right);
}

#[test]
fn reentering_a_branch_restores_its_last_focus_instead_of_nearest_geometry() {
    let mut app = app();
    let left = window(&mut app, 1);
    let top = window(&mut app, 2);
    split(&mut app, SplitAxis::Vertical);
    let middle = window(&mut app, 3);
    let bottom = window(&mut app, 4);
    // The middle window is geometrically closest to the center of the left one.
    focus(&mut app, bottom);
    navigate(&mut app, Direction::Left);
    assert_eq!(selected(&app), left);
    navigate(&mut app, Direction::Right);
    assert_eq!(selected(&app), bottom);
    focus(&mut app, top); // A click or other manager input also updates history.
    focus(&mut app, left);
    navigate(&mut app, Direction::Right);
    assert_eq!(selected(&app), top);
    assert_ne!(selected(&app), middle);
}

#[test]
fn ancestor_neighbor_wins_over_wrapping_unless_forced() {
    for (axis, direction) in [
        (SplitAxis::Horizontal, Direction::Right),
        (SplitAxis::Vertical, Direction::Down),
    ] {
        for mode in [
            FocusWrapping::No,
            FocusWrapping::Yes,
            FocusWrapping::Force,
            FocusWrapping::Workspace,
        ] {
            let mut app = app();
            app.insert_resource(mode);
            let first = window(&mut app, 1);
            split(&mut app, axis);
            let outside = window(&mut app, 2);
            focus(&mut app, first);
            split(&mut app, axis);
            let inner = window(&mut app, 3);
            navigate(&mut app, direction);
            assert_eq!(
                selected(&app),
                if mode == FocusWrapping::Force {
                    first
                } else {
                    outside
                }
            );
            focus(&mut app, inner);
            navigate(
                &mut app,
                if direction == Direction::Right {
                    Direction::Down
                } else {
                    Direction::Right
                },
            );
            assert_eq!(selected(&app), inner);
        }
    }
}

#[test]
fn closing_the_bottom_branch_restores_the_top_branches_last_window() {
    let mut app = app();
    let first = window(&mut app, 1);
    split(&mut app, SplitAxis::Vertical);
    let bottom = window(&mut app, 2);
    focus(&mut app, first);
    split(&mut app, SplitAxis::Horizontal);
    let middle = window(&mut app, 3);
    let right = window(&mut app, 4);
    navigate(&mut app, Direction::Down);
    assert_eq!(selected(&app), bottom);
    app.world_mut().despawn(bottom);
    app.update();
    assert_eq!(selected(&app), right);
    focus(&mut app, middle);
    app.world_mut().despawn(right);
    app.update();
    assert_eq!(selected(&app), middle);
}

#[test]
fn close_recovery_prefers_its_own_branch_over_more_recent_external_focus() {
    let mut app = app();
    let outside = window(&mut app, 1);
    let inside = window(&mut app, 2);
    split(&mut app, SplitAxis::Vertical);
    let closing = window(&mut app, 3);
    focus(&mut app, inside);
    focus(&mut app, outside);
    focus(&mut app, closing);
    app.world_mut().despawn(closing);
    app.update();
    assert_eq!(selected(&app), inside);
    // The unary branch was collapsed, so recovery must refresh its ancestry.
    app.world_mut().despawn(inside);
    app.update();
    assert_eq!(selected(&app), outside);
    app.world_mut().despawn(outside);
    app.update();
    assert_eq!(app.world().resource::<FocusedWindow>().entity(), None);
    assert_eq!(
        app.world()
            .resource::<TileFocusHistory>()
            .recent()
            .filter(|node| app.world().get::<ManagedWindow>(*node).is_some())
            .count(),
        0
    );
}

#[test]
fn retained_vacancies_are_navigable_without_client_keyboard_focus() {
    let mut app = app();
    let placeholder = window(&mut app, 1);
    window(&mut app, 2);
    let parent = *app.world().get::<TileParent>(placeholder).expect("parent");
    take_surface_actions(app.world_mut());
    navigate(&mut app, Direction::Left);
    app.update();
    assert_eq!(selected(&app), placeholder);
    assert_eq!(app.world().get::<TileParent>(placeholder), Some(&parent));
    assert!(
        take_surface_actions(app.world_mut()).contains(&SurfaceAction::Focus { surface: None })
    );
}

#[test]
fn inactive_branch_keeps_its_recency_when_its_last_focused_leaf_closes() {
    let mut app = app();
    let first = window(&mut app, 1);
    let outside = window(&mut app, 2);
    let closing = window(&mut app, 3);
    focus(&mut app, first);
    split(&mut app, SplitAxis::Vertical);
    let last_in_branch = window(&mut app, 4);
    // Root history is now branch, outside, closing, with first older than outside.
    focus(&mut app, outside);
    focus(&mut app, last_in_branch);
    focus(&mut app, closing);
    app.world_mut().despawn(last_in_branch);
    app.update();
    assert_eq!(selected(&app), closing);
    app.world_mut().despawn(closing);
    app.update();
    assert_eq!(selected(&app), first);
}

#[test]
fn same_batch_focus_changes_are_all_recorded() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    let third = window(&mut app, 3);
    app.world_mut()
        .run_system_once(|mut commands: Commands| {
            commands.trigger(I3FocusRequest(Direction::Left));
            commands.trigger(I3FocusRequest(Direction::Left));
        })
        .expect("queued focus batch");
    assert_eq!(selected(&app), first);
    assert_eq!(
        app.world()
            .resource::<TileFocusHistory>()
            .recent()
            .filter(|node| app.world().get::<ManagedWindow>(*node).is_some())
            .collect::<Vec<_>>(),
        [first, second, third]
    );
    app.world_mut().despawn(first);
    app.update();
    assert_eq!(selected(&app), second);
}

#[test]
fn late_vacancy_cleanup_keeps_recovery_path_after_unrelated_tree_edit() {
    let mut app = app();
    window(&mut app, 1);
    let sibling = window(&mut app, 2);
    split(&mut app, SplitAxis::Vertical);
    let closing = window(&mut app, 3);
    app.world_mut().trigger(TileRequest::Window(TileCommand {
        window: WindowId::new(1),
        operation: TileOperation::Split(SplitAxis::Vertical),
    }));
    app.world_mut().flush();
    // WindowPlugin despawns this vacancy in UiReconcile, after management.
    app.world_mut()
        .entity_mut(closing)
        .insert(WindowVacancy::Remove);
    app.update();
    assert!(app.world().get_entity(closing).is_err());
    app.update();
    assert_eq!(selected(&app), sibling);
}

#[test]
fn startup_focus_and_structure_requests_share_arrival_order() {
    for focus_first in [false, true] {
        let mut app = App::new();
        app.init_resource::<SurfaceActionQueue>().add_plugins((
            WindowPlugin,
            TilePlugin,
            I3QuirksPlugin,
        ));
        let first = window(&mut app, 1);
        let second = window(&mut app, 2);
        if focus_first {
            navigate(&mut app, Direction::Left);
        }
        app.world_mut()
            .trigger(TileRequest::Focused(TileOperation::Split(
                SplitAxis::Vertical,
            )));
        app.world_mut().flush();
        if !focus_first {
            navigate(&mut app, Direction::Left);
        }
        output(&mut app);
        app.update();
        assert_eq!(selected(&app), first);
        for (window, expected) in [(first, focus_first), (second, !focus_first)] {
            let parent = app
                .world()
                .get::<TileParent>(window)
                .expect("parent")
                .entity();
            assert_eq!(
                app.world()
                    .get::<TileContainer>(parent)
                    .expect("container")
                    .axis(),
                if expected {
                    SplitAxis::Vertical
                } else {
                    SplitAxis::Horizontal
                }
            );
        }
    }
}

#[test]
fn moving_the_focused_leaf_updates_its_recovery_ancestry() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    split(&mut app, SplitAxis::Vertical);
    let third = window(&mut app, 3);
    focus(&mut app, second);
    focus(&mut app, first);
    app.world_mut()
        .trigger(TileRequest::Focused(TileOperation::Move(Direction::Right)));
    app.update();
    // First is now inside the right branch with third; second is outside.
    // No focus command after the move may be needed to refresh this ancestry.
    app.world_mut().despawn(first);
    app.update();
    assert_eq!(selected(&app), third);
}

#[test]
fn transferred_windows_are_not_selected_from_old_history() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    let third = window(&mut app, 3);
    let manager = app.world_mut().spawn_empty().id();
    app.world_mut()
        .entity_mut(second)
        .insert(ManagedBy(manager));
    app.world_mut().despawn(third);
    app.update();
    assert_eq!(selected(&app), first);
}

#[test]
fn requests_wait_for_initial_workspace_admission() {
    let mut app = App::new();
    app.init_resource::<SurfaceActionQueue>().add_plugins((
        WindowPlugin,
        TilePlugin,
        I3QuirksPlugin,
    ));
    let first = window(&mut app, 1);
    window(&mut app, 2);
    navigate(&mut app, Direction::Left);
    output(&mut app);
    app.update();
    assert_eq!(selected(&app), first);
}

#[test]
fn navigation_does_not_rearrange_or_resize_windows() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    for _ in 0..10 {
        navigate(&mut app, Direction::Right);
    }
    assert_eq!(
        app.world()
            .get::<WindowGeometry>(first)
            .expect("geometry")
            .position,
        Vec2::ZERO
    );
    assert_eq!(
        app.world()
            .get::<WindowGeometry>(second)
            .expect("geometry")
            .position,
        Vec2::new(400.0, 0.0)
    );
}
