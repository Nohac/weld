//! Config rules apply to the matched occupant before layout and input publication.

use bevy::{
    app::App,
    ecs::entity::Entity,
    math::{UVec2, Vec2},
};
use indoc::indoc;
use weld_app::{
    output::{OutputGeometry, OutputId, PrimaryOutput, WeldOutput},
    surface::{
        ClientProvenance, ClientSource, ClientToplevel, ClientToplevelHints, ClientWindowMetadata,
        MappedSurface, SurfaceAction, SurfaceActionQueue, SurfaceId, take_surface_actions,
    },
};
use weld_client::{ClientSurfaceMetadata, ToplevelHints, ToplevelKind};
use weld_float::FloatBehaviorPlugin;
use weld_i3_quirks::{
    I3QuirksPlugin,
    config::{self, Configuration},
    workspace::{I3WorkspaceRequest, WorkspaceTarget},
};
use weld_ssd::{BorderStyle, WindowBorderStyle};
use weld_tile::{TileParent, TilePlugin, TileSettings};
use weld_window::{
    FloatingWindow, FocusedWindow, OccupiesWindow, WindowCommand, WindowCommandKind,
    WindowGeometry, WindowPlugin, WindowVisibility,
    workspace::{FocusedWorkspace, Workspace, WorkspaceMember},
};

fn config(source: &str) -> Configuration {
    config::parse_with_extensions("fixture", source, |_| Err(config::unsupported("command")))
        .expect("config")
}

fn app(source: &str) -> App {
    let mut app = App::new();
    app.init_resource::<SurfaceActionQueue>()
        .add_plugins((
            WindowPlugin,
            TilePlugin,
            I3QuirksPlugin,
            FloatBehaviorPlugin,
        ))
        .insert_resource(config(source).window_rules)
        .insert_resource(TileSettings {
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

fn metadata(app_id: &str, title: &str) -> ClientWindowMetadata {
    ClientWindowMetadata(ClientSurfaceMetadata::new(app_id.into(), title.into()).expect("metadata"))
}

fn client(app: &mut App, id: u64, app_id: &str, title: &str, kind: ToplevelKind) -> Entity {
    let surface = SurfaceId::for_test(id);
    app.world_mut()
        .spawn((
            ClientSource {
                id: surface.source(),
                provenance: ClientProvenance::Local,
            },
            ClientToplevel { surface },
            metadata(app_id, title),
            ClientToplevelHints(ToplevelHints {
                kind,
                ..Default::default()
            }),
            MappedSurface {
                logical_size: Vec2::new(300.0, 200.0),
                visual_size: Vec2::new(300.0, 200.0),
                visual_offset: Vec2::ZERO,
                opaque: true,
                alpha_mode: Default::default(),
            },
        ))
        .id()
}

fn window(app: &App, client: Entity) -> Entity {
    app.world()
        .get::<OccupiesWindow>(client)
        .expect("admitted")
        .0
}

fn workspace(app: &App, window: Entity) -> &str {
    let member = app
        .world()
        .get::<WorkspaceMember>(window)
        .expect("membership");
    app.world()
        .get::<Workspace>(member.0)
        .expect("workspace")
        .name()
}

#[test]
fn rule_sequence_places_background_window_before_first_publication() {
    let mut app = app(indoc! {r#"
        for_window [app_id="^utility$"] border pixel 3, floating enable, move absolute position 40 60, move to workspace number 3
        no_focus [app_id="^utility$"]
    "#});
    let main = client(&mut app, 1, "editor", "Main", ToplevelKind::Normal);
    app.update();
    let main = window(&app, main);
    let utility = client(&mut app, 2, "utility", "Tools", ToplevelKind::Normal);
    app.update();
    let utility = window(&app, utility);
    assert_eq!(workspace(&app, main), "1");
    assert_eq!(workspace(&app, utility), "3");
    assert_eq!(app.world().resource::<FocusedWindow>().entity(), Some(main));
    assert_eq!(
        app.world().get::<WindowVisibility>(utility),
        Some(&WindowVisibility::Hidden)
    );
    assert!(app.world().get::<FloatingWindow>(utility).is_some());
    assert_eq!(
        app.world().get::<WindowBorderStyle>(utility),
        Some(&WindowBorderStyle(BorderStyle::Pixel(3)))
    );
    assert_eq!(
        app.world()
            .get::<WindowGeometry>(utility)
            .expect("geometry")
            .position,
        Vec2::new(40.0, 60.0)
    );
    app.world_mut()
        .trigger(I3WorkspaceRequest::Switch(WorkspaceTarget::Number(
            "3".into(),
        )));
    app.update();
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(utility)
    );
    assert!(
        take_surface_actions(app.world_mut()).contains(&SurfaceAction::Focus {
            surface: Some(SurfaceId::for_test(2)),
        })
    );
}

#[test]
fn focus_mode_toggle_finds_a_floating_window_that_has_never_been_focused() {
    let mut app = app("for_window [app_id=utility] floating enable\nno_focus [app_id=utility]");
    let main_client = client(&mut app, 1, "editor", "Main", ToplevelKind::Normal);
    app.update();
    let main = window(&app, main_client);
    let utility_client = client(&mut app, 2, "utility", "Tools", ToplevelKind::Normal);
    app.update();
    let utility = window(&app, utility_client);
    assert_eq!(app.world().resource::<FocusedWindow>().entity(), Some(main));
    app.world_mut().trigger(weld_i3_quirks::I3FocusModeToggle);
    app.update();
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(utility)
    );
    app.world_mut().trigger(weld_i3_quirks::I3FocusModeToggle);
    app.update();
    assert_eq!(app.world().resource::<FocusedWindow>().entity(), Some(main));
}

#[test]
fn no_focus_preserves_existing_focus_but_allows_explicit_focus_and_sole_window() {
    let source = "for_window [app_id=utility] floating enable\nno_focus [app_id=utility]";
    let mut app = app(source);
    let utility = client(&mut app, 1, "utility", "Tools", ToplevelKind::Normal);
    app.update();
    let first = window(&app, utility);
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(first)
    );
    let second_client = client(&mut app, 2, "utility", "Another", ToplevelKind::Normal);
    app.update();
    let second = window(&app, second_client);
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(first)
    );
    app.world_mut().trigger(WindowCommand {
        window: second,
        kind: WindowCommandKind::Focus,
    });
    app.update();
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(second)
    );
}

#[test]
fn assignments_use_first_match_and_preserve_numbered_workspace_identity() {
    let mut app = app(indoc! {r#"
        assign [app_id=tool] → workspace number 3
        assign [app_id=tool] workspace 4
        assign [app_id=other] "5: a,b"
    "#});
    app.update();
    app.world_mut()
        .trigger(I3WorkspaceRequest::Switch(WorkspaceTarget::Name(
            "3:tools".into(),
        )));
    app.update();
    let tool_client = client(&mut app, 1, "tool", "Tools", ToplevelKind::Normal);
    app.update();
    let tool = window(&app, tool_client);
    assert_eq!(workspace(&app, tool), "3:tools");
    let other = client(&mut app, 2, "other", "Other", ToplevelKind::Normal);
    app.update();
    assert_eq!(workspace(&app, window(&app, other)), "5: a,b");
    assert_eq!(app.world().resource::<FocusedWindow>().entity(), Some(tool));
    app.insert_resource(config("assign [app_id=tool] workspace 8").window_rules);
    app.update();
    assert_eq!(workspace(&app, tool), "3:tools");
}

#[test]
fn late_title_rules_target_background_window_once_and_reload_reapplies_commands() {
    let source = "for_window [app_id=tool title=Ready] floating enable, move to workspace 2";
    let mut app = app(source);
    let tool_client = client(&mut app, 1, "tool", "Starting", ToplevelKind::Normal);
    app.update();
    let tool = window(&app, tool_client);
    let main_client = client(&mut app, 2, "editor", "Main", ToplevelKind::Normal);
    app.update();
    let main = window(&app, main_client);
    let selected = app.world().resource::<FocusedWorkspace>().entity();
    app.world_mut()
        .entity_mut(tool_client)
        .insert(metadata("tool", "Ready"));
    app.update();
    assert_eq!(workspace(&app, tool), "2");
    assert!(app.world().get::<FloatingWindow>(tool).is_some());
    assert_eq!(app.world().resource::<FocusedWindow>().entity(), Some(main));
    assert_eq!(
        app.world().resource::<FocusedWorkspace>().entity(),
        selected
    );
    app.world_mut()
        .trigger(weld_i3_quirks::workspace::I3WindowWorkspaceRequest {
            window: tool,
            target: WorkspaceTarget::Name("1".into()),
        });
    app.update();
    app.world_mut()
        .entity_mut(tool_client)
        .insert(metadata("tool", "Ready again"));
    app.update();
    assert_eq!(workspace(&app, tool), "1");
    app.insert_resource(config(source).window_rules);
    app.update();
    assert_eq!(workspace(&app, tool), "2");
    assert_eq!(app.world().resource::<FocusedWindow>().entity(), Some(main));
}

#[test]
fn explicit_tiling_overrides_dialog_hint_and_rule_removal_preserves_manual_state() {
    let mut app = app("for_window [app_id=dialog] floating disable");
    let dialog = client(&mut app, 1, "dialog", "Dialog", ToplevelKind::Dialog);
    app.update();
    let dialog = window(&app, dialog);
    assert!(app.world().get::<TileParent>(dialog).is_some());
    assert!(app.world().get::<FloatingWindow>(dialog).is_none());
    app.world_mut().trigger(weld_tile::TileFloatingRequest {
        window: Some(dialog),
        enabled: Some(true),
    });
    app.update();
    app.insert_resource(config("").window_rules);
    app.update();
    assert!(app.world().get::<FloatingWindow>(dialog).is_some());
}

#[test]
fn batch_admission_preserves_first_focus_and_reuses_assignment_destination() {
    let mut app = app(indoc! {r#"
        for_window [app_id=utility] floating enable
        no_focus [app_id=utility]
        assign [app_id=worker] workspace 3
    "#});
    let first = client(&mut app, 1, "utility", "First", ToplevelKind::Normal);
    client(&mut app, 2, "utility", "Second", ToplevelKind::Normal);
    let worker1 = client(&mut app, 3, "worker", "First worker", ToplevelKind::Normal);
    let worker2 = client(&mut app, 4, "worker", "Second worker", ToplevelKind::Normal);
    app.update();
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(window(&app, first))
    );
    let member1 = app
        .world()
        .get::<WorkspaceMember>(window(&app, worker1))
        .expect("first membership")
        .0;
    let member2 = app
        .world()
        .get::<WorkspaceMember>(window(&app, worker2))
        .expect("second membership")
        .0;
    assert_eq!(member1, member2);
    assert_eq!(workspace(&app, window(&app, worker1)), "3");
}
