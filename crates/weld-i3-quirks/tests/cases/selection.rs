//! Parent/child selection over nested splits, following i3's focus vocabulary.
use super::*;
use bevy::ecs::{observer::On, resource::Resource, system::ResMut};
use weld_i3_quirks::{I3FocusHierarchy, I3MoveRequest, I3StickyRequest};
use weld_tile::TileSelection;
use weld_window::workspace::{FocusedWorkspace, WorkspaceMember};
use weld_window::{
    StickyWindow, WindowGroupSelected, WindowPresentationOverride, WindowVisibility,
};

fn hierarchy(app: &mut App, request: I3FocusHierarchy) {
    app.world_mut().trigger(request);
    app.world_mut().flush();
}

fn group(app: &App) -> Option<Entity> {
    app.world().resource::<TileSelection>().container()
}

fn nested() -> (App, Entity, Entity, Entity, Entity) {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    split(&mut app, SplitAxis::Vertical);
    let third = window(&mut app, 3);
    let branch = app
        .world()
        .get::<TileParent>(third)
        .expect("branch")
        .entity();
    (app, first, second, third, branch)
}

#[test]
fn parent_child_retains_client_focus_and_highlights_only_the_selected_branch() {
    let (mut app, first, second, third, branch) = nested();
    hierarchy(&mut app, I3FocusHierarchy::Parent);
    assert_eq!(group(&app), Some(branch));
    assert_eq!(selected(&app), third);
    app.update();
    assert!(app.world().get::<WindowGroupSelected>(first).is_none());
    assert!(app.world().get::<WindowGroupSelected>(second).is_some());
    assert!(app.world().get::<WindowGroupSelected>(third).is_some());
    hierarchy(&mut app, I3FocusHierarchy::Parent);
    assert_eq!(
        group(&app),
        app.world().resource::<FocusedWorkspace>().entity()
    );
    hierarchy(&mut app, I3FocusHierarchy::Parent);
    hierarchy(&mut app, I3FocusHierarchy::Child);
    assert_eq!(group(&app), Some(branch));
    hierarchy(&mut app, I3FocusHierarchy::Child);
    assert_eq!(group(&app), None);
    assert_eq!(selected(&app), third);
    hierarchy(&mut app, I3FocusHierarchy::Child);
    assert_eq!(selected(&app), third);
}

#[test]
fn moving_and_resizing_a_selected_split_preserves_the_whole_subtree() {
    let (mut app, first, second, third, branch) = nested();
    hierarchy(&mut app, I3FocusHierarchy::Parent);
    app.world_mut().trigger(I3MoveRequest(Direction::Left));
    app.update();
    assert_eq!(group(&app), Some(branch));
    assert_eq!(selected(&app), third);
    for child in [second, third] {
        assert_eq!(
            app.world()
                .get::<TileParent>(child)
                .expect("parent")
                .entity(),
            branch
        );
    }
    let root = app.world().get::<TileParent>(first).expect("root").entity();
    assert_eq!(
        app.world()
            .get::<TileContainer>(root)
            .expect("root")
            .children()
            .map(|(child, _)| child)
            .collect::<Vec<_>>(),
        [branch, first]
    );
    let before = app
        .world()
        .get::<WindowGeometry>(third)
        .expect("geometry")
        .size
        .x;
    app.world_mut()
        .trigger(TileRequest::Focused(TileOperation::Resize {
            axis: SplitAxis::Horizontal,
            fraction: 0.1,
        }));
    app.update();
    assert!(
        app.world()
            .get::<WindowGeometry>(third)
            .expect("geometry")
            .size
            .x
            > before
    );
    assert_eq!(
        app.world()
            .get::<WindowGeometry>(second)
            .expect("geometry")
            .size
            .x,
        app.world()
            .get::<WindowGeometry>(third)
            .expect("geometry")
            .size
            .x
    );
}

#[test]
fn group_navigation_and_pointer_activation_return_to_leaf_selection() {
    let (mut app, first, _, third, branch) = nested();
    hierarchy(&mut app, I3FocusHierarchy::Parent);
    navigate(&mut app, Direction::Left);
    assert_eq!(selected(&app), first);
    assert_eq!(group(&app), None);
    focus(&mut app, third);
    hierarchy(&mut app, I3FocusHierarchy::Parent);
    assert_eq!(group(&app), Some(branch));
    app.world_mut().trigger(WindowIntent {
        window: third,
        kind: WindowIntentKind::Activate,
    });
    app.update();
    assert_eq!(group(&app), None);
}

#[test]
fn closing_a_child_recovers_focus_without_leaving_a_stale_group() {
    let (mut app, _, second, third, _) = nested();
    hierarchy(&mut app, I3FocusHierarchy::Parent);
    app.world_mut().despawn(third);
    app.update();
    assert_eq!(selected(&app), second);
    assert_eq!(group(&app), None);
}

#[test]
fn mode_toggle_from_a_group_selects_the_floating_window_then_the_remembered_leaf() {
    let (mut app, _, _, third, _) = nested();
    let floating = window(&mut app, 4);
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(floating),
        enabled: Some(true),
    });
    app.update();
    focus(&mut app, third);
    hierarchy(&mut app, I3FocusHierarchy::Parent);
    app.world_mut().trigger(I3FocusModeToggle);
    app.update();
    assert_eq!(selected(&app), floating);
    assert_eq!(group(&app), None);
    app.world_mut().trigger(I3FocusModeToggle);
    app.update();
    assert_eq!(selected(&app), third);
}

fn show(app: &mut App, name: &str) {
    app.world_mut()
        .trigger(I3WorkspaceRequest::Switch(WorkspaceTarget::Name(
            name.into(),
        )));
    app.update();
}

#[test]
fn sticky_floating_placeholder_follows_workspaces_and_disable_stops_it() {
    let mut app = app();
    let tile = window(&mut app, 1);
    let sticky = window(&mut app, 2);
    let presenter = app.world_mut().spawn_empty().id();
    app.world_mut()
        .entity_mut(sticky)
        .insert(WindowPresentationOverride::new(presenter));
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(sticky),
        enabled: Some(true),
    });
    app.world_mut().trigger(I3StickyRequest(None));
    app.update();
    assert!(app.world().get::<StickyWindow>(sticky).is_some());
    let geometry = *app.world().get::<WindowGeometry>(sticky).expect("geometry");
    show(&mut app, "2");
    assert_eq!(
        app.world()
            .get::<WorkspaceMember>(sticky)
            .expect("member")
            .0,
        app.world()
            .resource::<FocusedWorkspace>()
            .entity()
            .expect("workspace")
    );
    assert_eq!(
        app.world().get::<WindowVisibility>(sticky),
        Some(&WindowVisibility::Visible)
    );
    assert_eq!(
        app.world().get::<WindowVisibility>(tile),
        Some(&WindowVisibility::Hidden)
    );
    assert_eq!(
        *app.world().get::<WindowGeometry>(sticky).expect("geometry"),
        geometry
    );
    assert_eq!(selected(&app), sticky);
    assert!(
        app.world()
            .get::<WindowPresentationOverride>(sticky)
            .is_some()
    );
    app.world_mut().trigger(I3StickyRequest(None));
    show(&mut app, "1");
    assert_eq!(selected(&app), tile);
    assert_eq!(
        app.world().get::<WindowVisibility>(sticky),
        Some(&WindowVisibility::Hidden)
    );
}

#[test]
fn sticky_on_a_tiled_window_is_retained_but_only_takes_effect_when_floating() {
    let mut app = app();
    let tile = window(&mut app, 1);
    app.world_mut().trigger(I3StickyRequest(Some(true)));
    show(&mut app, "2");
    assert_eq!(
        app.world().get::<WindowVisibility>(tile),
        Some(&WindowVisibility::Hidden)
    );
    show(&mut app, "1");
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(tile),
        enabled: Some(true),
    });
    app.update();
    show(&mut app, "2");
    assert_eq!(selected(&app), tile);
    assert_eq!(
        app.world().get::<WindowVisibility>(tile),
        Some(&WindowVisibility::Visible)
    );
}

#[test]
fn moving_a_selected_group_to_another_workspace_preserves_internal_structure() {
    let (mut app, first, second, third, branch) = nested();
    let presenter = app.world_mut().spawn_empty().id();
    app.world_mut()
        .entity_mut(second)
        .insert(WindowPresentationOverride::new(presenter));
    let source = app.world().get::<WorkspaceMember>(first).expect("source").0;
    hierarchy(&mut app, I3FocusHierarchy::Parent);
    app.world_mut()
        .trigger(I3WorkspaceRequest::MoveWindow(WorkspaceTarget::Name(
            "2".into(),
        )));
    app.update();
    let destination = app
        .world()
        .get::<WorkspaceMember>(third)
        .expect("destination")
        .0;
    assert_ne!(source, destination);
    assert_eq!(
        app.world()
            .get::<WorkspaceMember>(second)
            .expect("member")
            .0,
        destination
    );
    assert_eq!(
        app.world()
            .get::<TileParent>(branch)
            .expect("branch root")
            .entity(),
        destination
    );
    for child in [second, third] {
        assert_eq!(
            app.world()
                .get::<TileParent>(child)
                .expect("parent")
                .entity(),
            branch
        );
        assert_eq!(
            app.world().get::<WindowVisibility>(child),
            Some(&WindowVisibility::Hidden)
        );
    }
    assert_eq!(selected(&app), first);
    assert_eq!(group(&app), None);
    assert!(
        app.world()
            .get::<WindowPresentationOverride>(second)
            .is_some()
    );
    show(&mut app, "2");
    assert_eq!(selected(&app), third);
}

#[test]
fn an_unfocused_sticky_window_does_not_steal_destination_focus() {
    let mut app = app();
    let source = window(&mut app, 1);
    let sticky = window(&mut app, 2);
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(sticky),
        enabled: Some(true),
    });
    app.world_mut().trigger(I3StickyRequest(Some(true)));
    app.update();
    show(&mut app, "2");
    let destination = window(&mut app, 3);
    show(&mut app, "1");
    focus(&mut app, source);
    show(&mut app, "2");
    assert_eq!(selected(&app), destination);
    assert_eq!(
        app.world().get::<WindowVisibility>(sticky),
        Some(&WindowVisibility::Visible)
    );
}

#[derive(Resource, Default)]
struct Closed(Vec<Entity>);

#[test]
fn close_targets_every_leaf_of_the_selected_group_but_not_its_neighbor() {
    let (mut app, first, second, third, _) = nested();
    app.init_resource::<Closed>();
    app.add_observer(|event: On<WindowCommand>, mut closed: ResMut<Closed>| {
        if event.kind == WindowCommandKind::CloseOccupant {
            closed.0.push(event.window);
        }
    });
    hierarchy(&mut app, I3FocusHierarchy::Parent);
    app.world_mut()
        .trigger(TileRequest::Focused(TileOperation::Close));
    app.world_mut().flush();
    let closed = &app.world().resource::<Closed>().0;
    assert_eq!(closed.len(), 2);
    assert!(closed.contains(&second) && closed.contains(&third));
    assert!(!closed.contains(&first));
}

#[test]
fn splitting_a_group_wraps_it_and_the_next_window_opens_beside_the_group() {
    let (mut app, _, second, third, branch) = nested();
    hierarchy(&mut app, I3FocusHierarchy::Parent);
    split(&mut app, SplitAxis::Horizontal);
    let wrapper = app
        .world()
        .get::<TileParent>(branch)
        .expect("wrapper")
        .entity();
    assert_eq!(
        app.world()
            .get::<TileContainer>(branch)
            .expect("old group")
            .axis(),
        SplitAxis::Vertical
    );
    assert_eq!(
        app.world()
            .get::<TileContainer>(wrapper)
            .expect("new group")
            .axis(),
        SplitAxis::Horizontal
    );
    let fourth = window(&mut app, 4);
    assert_eq!(
        app.world()
            .get::<TileParent>(fourth)
            .expect("new window parent")
            .entity(),
        wrapper
    );
    assert_eq!(
        app.world()
            .get::<TileContainer>(wrapper)
            .expect("new group")
            .children()
            .map(|(node, _)| node)
            .collect::<Vec<_>>(),
        [branch, fourth]
    );
    for child in [second, third] {
        assert_eq!(
            app.world()
                .get::<TileParent>(child)
                .expect("preserved child")
                .entity(),
            branch
        );
    }
    assert_eq!(group(&app), None);
    assert_eq!(selected(&app), fourth);
}

#[test]
fn container_navigation_preserves_group_level_at_neighbors_and_wrap_boundaries() {
    for wrapping in [FocusWrapping::Yes, FocusWrapping::Force] {
        let mut app = app();
        app.insert_resource(wrapping);
        let first = window(&mut app, 1);
        let second = window(&mut app, 2);
        focus(&mut app, first);
        split(&mut app, SplitAxis::Vertical);
        let third = window(&mut app, 3);
        let left = app
            .world()
            .get::<TileParent>(third)
            .expect("left split")
            .entity();
        focus(&mut app, second);
        split(&mut app, SplitAxis::Vertical);
        let fourth = window(&mut app, 4);
        let right = app
            .world()
            .get::<TileParent>(fourth)
            .expect("right split")
            .entity();
        hierarchy(&mut app, I3FocusHierarchy::Parent);
        assert_eq!(group(&app), Some(right));
        navigate(&mut app, Direction::Left);
        assert_eq!(group(&app), Some(left));
        assert_eq!(selected(&app), third);
        navigate(&mut app, Direction::Left);
        assert_eq!(group(&app), Some(right));
        assert_eq!(selected(&app), fourth);
    }
}

#[test]
fn selection_rejects_foreign_floating_and_stale_targets() {
    let (mut app, first, second, third, branch) = nested();
    focus(&mut app, first);
    app.world_mut().despawn(second);
    app.world_mut().despawn(third);
    app.world_mut().trigger(weld_tile::TileSelect(branch));
    app.world_mut().flush();
    assert_eq!(group(&app), None);
    assert_eq!(selected(&app), first);
    app.update();
    show(&mut app, "2");
    let other = window(&mut app, 4);
    app.world_mut().trigger(weld_tile::TileSelect(first));
    app.world_mut().flush();
    assert_eq!(selected(&app), other);
    app.world_mut().trigger(TileFloatingRequest {
        window: Some(other),
        enabled: Some(true),
    });
    app.update();
    app.world_mut().trigger(weld_tile::TileSelect(other));
    app.world_mut().flush();
    assert_eq!(group(&app), None);
    assert_eq!(selected(&app), other);
}
