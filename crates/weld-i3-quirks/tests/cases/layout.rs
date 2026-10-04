//! Adapted layout transitions from i3 192-layout.t and unary-split regressions.
//! See ../../UPSTREAM.md and ../../LICENSE-i3.

use super::*;
use weld_i3_quirks::{I3LayoutRequest, LayoutChoice};
use weld_tile::{TileHeaders, TileLayout, TileSelect};
use weld_window::{WindowVisibility, workspace::WorkspaceMember};

fn layout(app: &mut App, request: I3LayoutRequest) {
    app.world_mut().trigger(request);
    app.update();
}

fn parent(app: &App, node: Entity) -> Entity {
    app.world()
        .get::<TileParent>(node)
        .expect("parent")
        .entity()
}

fn mode(app: &App, node: Entity) -> TileLayout {
    app.world()
        .get::<TileContainer>(node)
        .expect("container")
        .layout()
}

fn visible(app: &App, window: Entity) -> bool {
    app.world().get::<WindowVisibility>(window) == Some(&WindowVisibility::Visible)
}

#[test]
fn layout_transitions_restore_split_axis_shares_and_children() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    split(&mut app, SplitAxis::Vertical);
    let third = window(&mut app, 3);
    let group = parent(&app, third);
    app.world_mut()
        .trigger(TileRequest::Focused(TileOperation::Resize {
            axis: SplitAxis::Vertical,
            fraction: 0.1,
        }));
    app.update();
    let before: Vec<_> = app
        .world()
        .get::<TileContainer>(group)
        .expect("group")
        .children()
        .collect();
    for (request, expected) in [
        (
            I3LayoutRequest::Set(TileLayout::Stacked),
            TileLayout::Stacked,
        ),
        (I3LayoutRequest::Set(TileLayout::Tabbed), TileLayout::Tabbed),
        (
            I3LayoutRequest::Cycle(vec![LayoutChoice::Split]),
            TileLayout::Split(SplitAxis::Vertical),
        ),
        (
            I3LayoutRequest::Cycle(vec![LayoutChoice::Split]),
            TileLayout::Split(SplitAxis::Horizontal),
        ),
        (I3LayoutRequest::Toggle, TileLayout::Stacked),
        (I3LayoutRequest::Toggle, TileLayout::Tabbed),
        (
            I3LayoutRequest::Toggle,
            TileLayout::Split(SplitAxis::Horizontal),
        ),
        (
            I3LayoutRequest::ToggleAll,
            TileLayout::Split(SplitAxis::Vertical),
        ),
        (I3LayoutRequest::ToggleAll, TileLayout::Stacked),
        (I3LayoutRequest::ToggleAll, TileLayout::Tabbed),
    ] {
        layout(&mut app, request);
        assert_eq!(mode(&app, group), expected);
        assert_eq!(
            app.world()
                .get::<TileContainer>(group)
                .expect("group")
                .children()
                .collect::<Vec<_>>(),
            before
        );
        assert_eq!(selected(&app), third);
        assert!(visible(&app, first));
    }
    assert!(!visible(&app, second));
    assert!(visible(&app, third));
}

#[test]
fn tab_focus_visibility_and_close_recovery_share_branch_history() {
    for mode in [TileLayout::Tabbed, TileLayout::Stacked] {
        let mut app = app();
        let first = window(&mut app, 1);
        let second = window(&mut app, 2);
        let third = window(&mut app, 3);
        layout(&mut app, I3LayoutRequest::Set(mode));
        let group = parent(&app, third);
        assert_eq!(parent(&app, first), group);
        assert_eq!(parent(&app, second), group);
        assert!(!visible(&app, first));
        assert!(!visible(&app, second));
        assert!(visible(&app, third));
        let backwards = if mode == TileLayout::Tabbed {
            Direction::Left
        } else {
            Direction::Up
        };
        navigate(&mut app, backwards);
        app.update();
        assert_eq!(selected(&app), second);
        assert!(visible(&app, second));
        assert!(!visible(&app, third));
        let headers = &app.world().get::<TileHeaders>(group).expect("headers").0;
        assert_eq!(headers.len(), 3);
        assert_eq!(
            headers
                .iter()
                .find(|header| header.selected)
                .expect("active")
                .child,
            second
        );
        app.world_mut().entity_mut(second).despawn();
        app.update();
        assert_eq!(selected(&app), third);
        assert!(visible(&app, third));
        assert!(!visible(&app, first));
    }
}

#[test]
fn nested_tab_hides_entire_inactive_branch_and_restores_its_focus() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    layout(&mut app, I3LayoutRequest::Set(TileLayout::Tabbed));
    split(&mut app, SplitAxis::Vertical);
    let third = window(&mut app, 3);
    assert!(visible(&app, second) && visible(&app, third));
    assert!(!visible(&app, first));
    navigate(&mut app, Direction::Left);
    app.update();
    assert_eq!(selected(&app), first);
    assert!(!visible(&app, second) && !visible(&app, third));
    navigate(&mut app, Direction::Right);
    app.update();
    assert_eq!(selected(&app), third);
    assert!(visible(&app, second) && visible(&app, third));
}

#[test]
fn tab_and_stack_movement_reorders_then_extracts_at_the_edge() {
    for mode in [TileLayout::Tabbed, TileLayout::Stacked] {
        let mut app = app();
        let first = window(&mut app, 1);
        let second = window(&mut app, 2);
        let third = window(&mut app, 3);
        layout(&mut app, I3LayoutRequest::Set(mode));
        let group = parent(&app, third);
        let backwards = if mode == TileLayout::Tabbed {
            Direction::Left
        } else {
            Direction::Up
        };
        app.world_mut()
            .trigger(weld_i3_quirks::I3MoveRequest(backwards));
        app.update();
        assert_eq!(
            app.world()
                .get::<TileContainer>(group)
                .expect("group")
                .children()
                .map(|(child, _)| child)
                .collect::<Vec<_>>(),
            [first, third, second]
        );
        app.world_mut()
            .trigger(weld_i3_quirks::I3MoveRequest(backwards));
        app.update();
        app.world_mut()
            .trigger(weld_i3_quirks::I3MoveRequest(backwards));
        app.update();
        assert_ne!(parent(&app, third), group);
        assert_eq!(parent(&app, first), group);
        assert!(visible(&app, third));
    }
}

#[test]
fn layout_changes_target_the_parent_of_a_selected_group() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    split(&mut app, SplitAxis::Vertical);
    let third = window(&mut app, 3);
    let inner = parent(&app, third);
    app.world_mut().trigger(TileSelect(inner));
    app.update();
    layout(&mut app, I3LayoutRequest::Set(TileLayout::Tabbed));
    let outer = parent(&app, inner);
    assert_eq!(mode(&app, outer), TileLayout::Tabbed);
    assert_eq!(mode(&app, inner), TileLayout::Split(SplitAxis::Vertical));
    assert!(!visible(&app, first));
    assert!(visible(&app, second) && visible(&app, third));
}

#[test]
fn workspace_transfer_keeps_tab_layout_and_active_child() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    layout(&mut app, I3LayoutRequest::Set(TileLayout::Tabbed));
    let group = parent(&app, first);
    app.world_mut().trigger(TileSelect(group));
    app.update();
    app.world_mut()
        .trigger(I3WorkspaceRequest::MoveWindow(WorkspaceTarget::Name(
            "2".into(),
        )));
    app.update();
    assert!(!visible(&app, first) && !visible(&app, second));
    app.world_mut()
        .trigger(I3WorkspaceRequest::Switch(WorkspaceTarget::Name(
            "2".into(),
        )));
    app.update();
    assert_eq!(mode(&app, group), TileLayout::Tabbed);
    assert_eq!(selected(&app), second);
    assert!(!visible(&app, first) && visible(&app, second));
    assert_eq!(
        app.world().get::<WorkspaceMember>(first),
        app.world().get::<WorkspaceMember>(second)
    );
}

#[test]
fn repeated_unary_split_and_tab_commands_do_not_accumulate_containers() {
    let mut app = app();
    let only = window(&mut app, 1);
    for _ in 0..100 {
        layout(&mut app, I3LayoutRequest::Set(TileLayout::Stacked));
        split(&mut app, SplitAxis::Vertical);
        layout(&mut app, I3LayoutRequest::Set(TileLayout::Tabbed));
        split(&mut app, SplitAxis::Horizontal);
        let mut depth = 0;
        let mut node = only;
        while let Some(parent) = app.world().get::<TileParent>(node) {
            depth += 1;
            node = parent.entity();
        }
        assert!(depth <= 3, "unary depth {depth}");
    }
}

#[test]
fn closing_to_one_tab_preserves_layout_for_the_next_window() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    layout(&mut app, I3LayoutRequest::Set(TileLayout::Tabbed));
    let group = parent(&app, second);
    app.world_mut().entity_mut(second).despawn();
    app.update();
    assert_eq!(mode(&app, group), TileLayout::Tabbed);
    assert_eq!(selected(&app), first);
    let third = window(&mut app, 3);
    assert_eq!(parent(&app, third), group);
    assert!(!visible(&app, first) && visible(&app, third));
}

#[test]
fn default_layout_on_a_populated_workspace_preserves_its_children() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    let root = parent(&app, second);
    layout(&mut app, I3LayoutRequest::Default);
    assert_eq!(parent(&app, first), root);
    assert_eq!(parent(&app, second), root);
    assert_eq!(
        app.world()
            .get::<TileContainer>(root)
            .expect("root")
            .children()
            .count(),
        2
    );
}

#[test]
fn stacking_navigation_uses_layout_axis_after_parent_split_changes() {
    let mut app = app();
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    layout(&mut app, I3LayoutRequest::Set(TileLayout::Stacked));
    navigate(&mut app, Direction::Down);
    assert_eq!(selected(&app), first);
    navigate(&mut app, Direction::Up);
    assert_eq!(selected(&app), second);
    let group = parent(&app, second);
    app.world_mut().trigger(TileSelect(group));
    app.update();
    split(&mut app, SplitAxis::Horizontal);
    app.world_mut()
        .trigger(weld_i3_quirks::I3FocusHierarchy::Child);
    app.update();
    navigate(&mut app, Direction::Down);
    assert_eq!(selected(&app), first);
    navigate(&mut app, Direction::Up);
    assert_eq!(selected(&app), second);
}
