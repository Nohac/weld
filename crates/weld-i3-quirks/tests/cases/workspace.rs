//! Workspace scenarios from i3 297, 503 and the explicit-command part of 176.
use super::*;
use bevy::ecs::query::With;
use indoc::indoc;
use weld_app::output::OutputInfo;
use weld_i3_quirks::{
    config,
    workspace::{I3WorkspaceRequest, WorkspaceTarget},
};
use weld_window::workspace::{
    FocusedWorkspace, OutputWorkspaces, Workspace, WorkspaceMember, WorkspaceOutput,
    WorkspaceRequest, WorkspaceWindows,
};
use weld_window::{OccupiesWindow, WindowOutput, WindowPresentationOverride, WindowVisibility};

fn parse_config(name: &str, source: &str) -> anyhow::Result<config::Configuration> {
    config::parse_with_extensions(name, source, |_| anyhow::bail!("unsupported i3 command"))
}

fn named_output(app: &mut App, id: u64, name: &str, primary: bool) -> Entity {
    let mut entity = app.world_mut().spawn((
        WeldOutput {
            id: OutputId::new(id),
        },
        OutputInfo::for_test(name),
        OutputGeometry::from_physical(UVec2::new(800 + id as u32 * 100, 600), 1.0),
    ));
    if primary {
        entity.insert(PrimaryOutput);
    }
    entity.id()
}

fn configured(source: &str, count: u64) -> (App, Vec<Entity>) {
    let mut app = App::new();
    app.init_resource::<SurfaceActionQueue>().add_plugins((
        WindowPlugin,
        TilePlugin,
        I3QuirksPlugin,
    ));
    app.insert_resource(
        parse_config("test", source)
            .expect("configuration")
            .workspaces,
    );
    let outputs = (0..count)
        .map(|id| named_output(&mut app, id + 1, &format!("fake-{id}"), id == 0))
        .collect();
    app.update();
    (app, outputs)
}

fn workspace(app: &mut App, name: &str) -> Option<Entity> {
    app.world_mut()
        .query::<(Entity, &Workspace)>()
        .iter(app.world())
        .find(|(_, workspace)| workspace.name() == name)
        .map(|(entity, _)| entity)
}

fn current(app: &App) -> String {
    let entity = app
        .world()
        .resource::<FocusedWorkspace>()
        .entity()
        .expect("selected workspace");
    app.world()
        .get::<Workspace>(entity)
        .expect("live workspace")
        .name()
        .to_owned()
}

fn switch(app: &mut App, target: WorkspaceTarget) {
    app.world_mut().trigger(I3WorkspaceRequest::Switch(target));
    app.update();
}

fn show(app: &mut App, name: &str) {
    switch(app, WorkspaceTarget::Name(name.to_owned()));
}

fn move_to(app: &mut App, name: &str) {
    app.world_mut()
        .trigger(I3WorkspaceRequest::MoveWindow(WorkspaceTarget::Name(
            name.to_owned(),
        )));
    app.update();
}

fn assigned(app: &mut App, name: &str) -> Option<Entity> {
    workspace(app, name).and_then(|workspace| {
        app.world()
            .get::<WorkspaceOutput>(workspace)
            .map(|output| output.0)
    })
}

#[test]
fn upstream_297_initial_assignments_bindings_and_unused_numbers() {
    let (mut app, outputs) = configured(
        indoc! {"
            bindsym Mod1+x workspace bindingname
            workspace 9 output doesnotexist
            workspace special output fake-0
            workspace 1 output doesnotexist
            workspace dontusethisname output doesnotexist
            workspace donotoverride output fake-0
            workspace 2 output fake-0
            workspace 3 output fake-0
        "},
        4,
    );
    for (name, output) in [
        ("special", outputs[0]),
        ("bindingname", outputs[1]),
        ("1", outputs[2]),
        ("4", outputs[3]),
    ] {
        assert_eq!(assigned(&mut app, name), Some(output));
    }
    for absent in ["9", "2", "3", "dontusethisname", "donotoverride"] {
        assert!(workspace(&mut app, absent).is_none());
    }
    assert_eq!(current(&app), "special");
}

#[test]
fn upstream_297_connector_preferences_and_exact_name_precedence() {
    let (mut app, outputs) = configured(
        indoc! {r#"
            workspace 1 output fake-0
            workspace 2 output missing fake-1 fake-0
            workspace 5 output fake-0
            workspace "5:work" output fake-1
            workspace "6:work" output fake-0
            workspace 6 output fake-1
            workspace 7 output nonprimary primary
            workspace 8 output missing primary
        "#},
        2,
    );
    for (name, output) in [
        ("1", outputs[0]),
        ("2", outputs[1]),
        ("5", outputs[0]),
        ("5:work", outputs[1]),
        ("6", outputs[1]),
        ("6:work", outputs[0]),
        ("7", outputs[1]),
        ("8", outputs[0]),
    ] {
        show(&mut app, name);
        assert_eq!(assigned(&mut app, name), Some(output), "{name}");
    }
    show(&mut app, "2");
    show(&mut app, "unassigned");
    assert_eq!(assigned(&mut app, "unassigned"), Some(outputs[1]));
}

#[test]
fn switching_preserves_each_tree_geometry_membership_and_focus() {
    let (mut app, _) = configured("", 1);
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    focus(&mut app, first);
    let before = *app.world().get::<WindowGeometry>(first).expect("geometry");
    let tree = app.world().get::<TileParent>(first).expect("tree").entity();
    show(&mut app, "2");
    assert!(app.world().resource::<FocusedWindow>().entity().is_none());
    let third = window(&mut app, 3);
    assert_eq!(
        *app.world()
            .get::<WindowVisibility>(first)
            .expect("visibility"),
        WindowVisibility::Hidden
    );
    show(&mut app, "1");
    assert_eq!(selected(&app), first);
    assert_eq!(
        *app.world().get::<WindowGeometry>(first).expect("geometry"),
        before
    );
    assert_eq!(
        app.world().get::<TileParent>(first).expect("tree").entity(),
        tree
    );
    assert_eq!(
        *app.world()
            .get::<WindowVisibility>(third)
            .expect("visibility"),
        WindowVisibility::Hidden
    );
    navigate(&mut app, Direction::Right);
    assert_eq!(selected(&app), second);
    show(&mut app, "2");
    assert_eq!(selected(&app), third);
}

#[test]
fn moving_updates_inverse_edges_and_restores_source_without_following() {
    let (mut app, outputs) = configured("workspace 2 output fake-1", 2);
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    let source = app
        .world()
        .get::<WorkspaceMember>(second)
        .expect("source")
        .0;
    move_to(&mut app, "2");
    let target = workspace(&mut app, "2").expect("destination");
    assert_eq!(selected(&app), first);
    assert_eq!(
        app.world()
            .get::<WorkspaceMember>(second)
            .expect("member")
            .0,
        target
    );
    assert_eq!(
        app.world().get::<ManagedBy>(second).expect("manager").0,
        target
    );
    assert!(
        !app.world()
            .get::<WorkspaceWindows>(source)
            .expect("source members")
            .iter()
            .any(|window| window == second)
    );
    assert!(
        app.world()
            .get::<WorkspaceWindows>(target)
            .expect("target members")
            .iter()
            .any(|window| window == second)
    );
    assert_eq!(
        app.world().get::<WindowOutput>(second).expect("output").0,
        outputs[1]
    );
    show(&mut app, "2");
    assert_eq!(selected(&app), second);
    assert_eq!(
        *app.world()
            .get::<WindowVisibility>(first)
            .expect("visible other monitor"),
        WindowVisibility::Visible
    );
}

#[test]
fn upstream_503_global_and_per_output_order_include_equal_numbers() {
    let (mut app, _) = configured("", 2);
    window(&mut app, 1);
    show(&mut app, "2");
    window(&mut app, 2);
    show(&mut app, "6:c");
    window(&mut app, 3);
    show(&mut app, "1");
    for (id, name) in [(4, "5"), (5, "6:a"), (6, "6:b")] {
        show(&mut app, name);
        window(&mut app, id);
    }
    show(&mut app, "1");
    for name in ["2", "5", "6:a", "6:b", "6:c", "1"] {
        switch(&mut app, WorkspaceTarget::Next);
        assert_eq!(current(&app), name);
    }
    for name in ["5", "6:a", "6:b", "1"] {
        switch(&mut app, WorkspaceTarget::NextOnOutput);
        assert_eq!(current(&app), name);
    }
    for name in ["6:b", "6:a", "5", "1"] {
        switch(&mut app, WorkspaceTarget::PreviousOnOutput);
        assert_eq!(current(&app), name);
    }
    show(&mut app, "2");
    switch(&mut app, WorkspaceTarget::PreviousOnOutput);
    assert_eq!(current(&app), "6:c");
}

#[test]
fn upstream_176_explicit_back_and_forth_recreates_empty_workspace() {
    let (mut app, _) = configured("", 1);
    let original = workspace(&mut app, "1").expect("initial");
    show(&mut app, "2");
    assert!(workspace(&mut app, "1").is_none());
    switch(&mut app, WorkspaceTarget::BackAndForth);
    assert_eq!(current(&app), "1");
    assert_ne!(workspace(&mut app, "1"), Some(original));
    show(&mut app, "1");
    switch(&mut app, WorkspaceTarget::BackAndForth);
    assert_eq!(current(&app), "2");
}

#[test]
fn source_slots_and_client_occupancy_survive_hidden_workspace() {
    let (mut app, _) = configured("", 1);
    let window = window(&mut app, 1);
    let owner = app.world_mut().spawn_empty().id();
    app.world_mut()
        .entity_mut(window)
        .insert(WindowPresentationOverride::new(owner));
    let client = app.world_mut().spawn(OccupiesWindow(window)).id();
    show(&mut app, "2");
    assert_eq!(
        app.world()
            .get::<OccupiesWindow>(client)
            .expect("occupant retained")
            .0,
        window
    );
    assert!(
        app.world()
            .get::<WindowPresentationOverride>(window)
            .is_some()
    );
    assert!(app.world().get::<TileParent>(window).is_some());
    show(&mut app, "1");
    assert_eq!(selected(&app), window);
}

#[test]
fn losing_output_preserves_workspace_and_window_then_rehomes_them() {
    let (mut app, outputs) = configured("", 2);
    show(&mut app, "2");
    let window = window(&mut app, 1);
    let workspace = workspace(&mut app, "2").expect("workspace");
    app.world_mut().despawn(outputs[1]);
    assert!(app.world().get::<ManagedWindow>(window).is_some());
    app.update();
    assert_eq!(
        app.world()
            .get::<WorkspaceOutput>(workspace)
            .expect("reassigned")
            .0,
        outputs[0]
    );
    assert_eq!(
        app.world()
            .get::<WindowOutput>(window)
            .expect("reassigned window")
            .0,
        outputs[0]
    );
    assert_eq!(selected(&app), window);
    assert_eq!(
        app.world_mut()
            .query_filtered::<&Workspace, With<WorkspaceOutput>>()
            .iter(app.world())
            .filter(|state| state.visible())
            .count(),
        1
    );
}

#[test]
fn assigning_workspace_updates_inverse_output_membership_and_layout() {
    let (mut app, outputs) = configured("", 2);
    let window = window(&mut app, 1);
    let workspace = workspace(&mut app, "1").expect("workspace");
    app.world_mut().trigger(WorkspaceRequest::Assign {
        workspace,
        output: outputs[1],
    });
    app.update();
    assert!(
        !app.world()
            .get::<OutputWorkspaces>(outputs[0])
            .expect("inverse")
            .iter()
            .any(|entity| entity == workspace)
    );
    assert!(
        app.world()
            .get::<OutputWorkspaces>(outputs[1])
            .expect("inverse")
            .iter()
            .any(|entity| entity == workspace)
    );
    assert_eq!(
        app.world().get::<WindowOutput>(window).expect("output").0,
        outputs[1]
    );
}

#[test]
fn ordered_switch_move_and_switch_see_published_memberships() {
    let (mut app, _) = configured("", 1);
    let window = window(&mut app, 1);
    app.world_mut()
        .run_system_once(|mut commands: Commands| {
        commands.trigger(I3WorkspaceRequest::MoveWindow(WorkspaceTarget::Name(
            "2".to_owned(),
        )));
        commands.queue(|world: &mut bevy::ecs::world::World| {
            let mut query = world.query::<(&WindowGeometry, &WorkspaceMember)>();
            assert!(query.iter(world).all(|(geometry, _)| geometry.size.min_element() > 1.0),
                "a move into a newly created workspace publishes usable geometry before the next action");
        });
            commands.trigger(I3WorkspaceRequest::Switch(WorkspaceTarget::Name(
                "2".to_owned(),
            )));
            commands.trigger(TileRequest::Focused(TileOperation::Split(
                SplitAxis::Vertical,
            )));
        })
        .expect("ordered actions");
    app.update();
    assert_eq!(selected(&app), window);
    let parent = app
        .world()
        .get::<TileParent>(window)
        .expect("parent")
        .entity();
    assert_eq!(
        app.world()
            .get::<TileContainer>(parent)
            .expect("root")
            .axis(),
        SplitAxis::Vertical
    );
}

#[test]
fn numeric_selection_and_reload_do_not_recreate_existing_workspaces() {
    let (mut app, outputs) = configured("", 2);
    show(&mut app, "3: work");
    let window = window(&mut app, 1);
    let original = workspace(&mut app, "3: work").expect("workspace");
    show(&mut app, "2");
    app.insert_resource(
        parse_config("reload", "workspace 3 output fake-1")
            .expect("config")
            .workspaces,
    );
    switch(&mut app, WorkspaceTarget::Number("3".to_owned()));
    assert_eq!(current(&app), "3: work");
    assert_eq!(selected(&app), window);
    assert_eq!(workspace(&mut app, "3: work"), Some(original));
    assert_eq!(assigned(&mut app, "3: work"), Some(outputs[0]));
    show(&mut app, "3: recording");
    assert_eq!(assigned(&mut app, "3: recording"), Some(outputs[1]));
}

#[test]
fn inactive_workspace_admission_does_not_take_focus() {
    let (mut app, _) = configured("", 1);
    let active = window(&mut app, 1);
    show(&mut app, "2");
    let other = window(&mut app, 2);
    let target = workspace(&mut app, "2").expect("target");
    show(&mut app, "1");
    let added = app
        .world_mut()
        .spawn((
            ManagedWindow {
                id: WindowId::new(3),
            },
            WindowVacancy::Retain,
            WorkspaceMember(target),
        ))
        .id();
    app.update();
    assert_eq!(selected(&app), active);
    assert_eq!(
        app.world()
            .get::<ManagedBy>(added)
            .expect("admitted to inactive workspace")
            .0,
        target
    );
    assert_eq!(
        *app.world().get::<WindowVisibility>(added).expect("hidden"),
        WindowVisibility::Hidden
    );
    assert_eq!(
        app.world()
            .get::<TileParent>(added)
            .expect("parent")
            .entity(),
        app.world()
            .get::<TileParent>(other)
            .expect("other parent")
            .entity()
    );
}

#[test]
fn invalid_transfer_leaves_both_layouts_and_membership_unchanged() {
    let (mut app, _) = configured("", 1);
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    let source = app.world().get::<WorkspaceMember>(first).expect("source").0;
    show(&mut app, "2");
    let third = window(&mut app, 3);
    let target = workspace(&mut app, "2").expect("target");
    for anchor in [Some(first), Some(second)] {
        app.world_mut().trigger(weld_tile::TileWorkspaceMove {
            window: first,
            workspace: target,
            anchor,
        });
        app.update();
        assert_eq!(
            app.world()
                .get::<WorkspaceMember>(first)
                .expect("source retained")
                .0,
            source
        );
        assert_eq!(
            app.world()
                .get::<TileContainer>(target)
                .expect("target tree")
                .children()
                .map(|(entity, _)| entity)
                .collect::<Vec<_>>(),
            [third]
        );
    }
    // Ordinary reparent edits cannot bypass the workspace transfer contract.
    app.world_mut().trigger(weld_tile::TileTreeEdit::Place {
        node: first,
        anchor: third,
        side: weld_tile::TileSide::After,
    });
    app.update();
    assert_eq!(
        app.world()
            .get::<WorkspaceMember>(first)
            .expect("source retained")
            .0,
        source
    );
    assert_eq!(
        app.world()
            .get::<TileContainer>(source)
            .expect("source tree")
            .children()
            .count(),
        2
    );
}

#[test]
fn deleting_an_inactive_selected_leaf_does_not_focus_its_workspace() {
    let (mut app, _) = configured("", 1);
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    show(&mut app, "2");
    let third = window(&mut app, 3);
    app.world_mut().despawn(second);
    app.update();
    assert_eq!(selected(&app), third);
    assert_eq!(current(&app), "2");
    show(&mut app, "1");
    assert_eq!(selected(&app), first);
}

#[test]
fn startup_workspace_switches_share_order_with_tiling_commands() {
    let mut app = App::new();
    app.init_resource::<SurfaceActionQueue>().add_plugins((
        WindowPlugin,
        TilePlugin,
        I3QuirksPlugin,
    ));
    app.world_mut()
        .trigger(I3WorkspaceRequest::Switch(WorkspaceTarget::Name(
            "3".to_owned(),
        )));
    let window = app
        .world_mut()
        .spawn((
            ManagedWindow {
                id: WindowId::new(1),
            },
            WindowVacancy::Retain,
        ))
        .id();
    named_output(&mut app, 1, "fake-0", true);
    app.update();
    assert_eq!(current(&app), "3");
    let original = workspace(&mut app, "1").expect("old populated workspace");
    assert_eq!(
        app.world()
            .get::<WorkspaceMember>(window)
            .expect("admitted before pending switch")
            .0,
        original
    );
    assert!(app.world().resource::<FocusedWindow>().entity().is_none());
}

#[test]
fn returning_after_inactive_close_restores_nearest_remembered_branch() {
    let (mut app, _) = configured("", 1);
    let outside = window(&mut app, 1);
    let sibling = window(&mut app, 2);
    split(&mut app, SplitAxis::Vertical);
    let lost = window(&mut app, 3);
    focus(&mut app, outside);
    focus(&mut app, lost);
    show(&mut app, "2");
    window(&mut app, 4);
    app.world_mut().despawn(lost);
    app.update();
    show(&mut app, "1");
    assert_eq!(selected(&app), sibling);
}

#[test]
fn transfer_restores_source_branch_and_remembers_destination_leaf() {
    let (mut app, _) = configured("", 1);
    let outside = window(&mut app, 1);
    let sibling = window(&mut app, 2);
    split(&mut app, SplitAxis::Vertical);
    let moved = window(&mut app, 3);
    focus(&mut app, outside);
    focus(&mut app, moved);
    move_to(&mut app, "2");
    assert_eq!(selected(&app), sibling);
    show(&mut app, "2");
    assert_eq!(selected(&app), moved);
}

#[test]
fn removing_every_output_keeps_windows_for_a_later_output() {
    let (mut app, outputs) = configured("", 1);
    let window = window(&mut app, 1);
    let workspace = workspace(&mut app, "1").expect("workspace");
    app.world_mut().despawn(outputs[0]);
    app.update();
    assert_eq!(
        *app.world().get::<WindowVisibility>(window).expect("hidden"),
        WindowVisibility::Hidden
    );
    assert_eq!(
        app.world()
            .get::<WorkspaceMember>(window)
            .expect("member")
            .0,
        workspace
    );
    let output = named_output(&mut app, 3, "replacement", true);
    app.update();
    assert_eq!(
        app.world().get::<WindowOutput>(window).expect("output").0,
        output
    );
    assert_eq!(
        *app.world()
            .get::<WindowVisibility>(window)
            .expect("visible"),
        WindowVisibility::Visible
    );
    assert_eq!(selected(&app), window);
}

#[test]
fn explicit_workspace_destruction_readmits_surviving_windows() {
    let (mut app, outputs) = configured("", 2);
    let first = window(&mut app, 1);
    let second = window(&mut app, 2);
    split(&mut app, SplitAxis::Vertical);
    let third = window(&mut app, 3);
    let deleted = workspace(&mut app, "1").expect("workspace");
    app.world_mut().despawn(deleted);
    app.update();
    for window in [first, second, third] {
        let member = app
            .world()
            .get::<WorkspaceMember>(window)
            .expect("readmitted");
        assert_ne!(member.0, deleted);
        assert_eq!(
            app.world().get::<ManagedBy>(window).expect("owned").0,
            member.0
        );
        assert!(app.world().get::<TileParent>(window).is_some());
        assert_eq!(
            app.world().get::<WindowOutput>(window).expect("assigned").0,
            outputs[0]
        );
    }
    assert!(app.world().resource::<FocusedWindow>().entity().is_some());
}
