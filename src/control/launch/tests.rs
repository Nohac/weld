use super::*;
use bevy::{asset::AssetApp, scene::ScenePlugin, window::RequestRedraw};
use weld_app::{
    output::{OutputGeometry, OutputId, OutputPosition, PrimaryOutput, WeldOutput},
    surface::{
        ClientDecorated, ClientSource, ClientToplevel, MappedSurface, SurfaceActionQueue,
        SurfacePlugin,
    },
};
use weld_client::{ClientProvenance, ClientSurfaceMetadata};
use weld_i3_quirks::{I3QuirksPlugin, workspace::I3WorkspaceRequest};
use weld_window::{
    FocusedWindow, ManagedBy, OccupiesWindow, PrimaryWindowPresentation, WindowHeaderRequested,
    WindowOccupant, WindowOutput, WindowPlugin, workspace::WorkspaceMember,
};

fn fixture() -> (App, mpsc::SyncSender<LaunchMessage>) {
    let (sender, receiver) = mpsc::sync_channel(32);
    let mut app = App::new();
    app.init_resource::<SurfaceActionQueue>()
        .insert_resource(SessionAccess(weld_access::AccessAuthority::new(|| {})))
        .insert_resource(Preparations(Mutex::new(receiver)))
        .init_resource::<Launches>()
        .add_plugins((WindowPlugin, weld_tile::TilePlugin, I3QuirksPlugin))
        .add_observer(dismiss)
        .add_systems(
            PreUpdate,
            (prepare, admit)
                .chain()
                .in_set(LaunchAdmission)
                .after(WindowSystems::AdmissionPolicy)
                .before(WindowSystems::PresentationRevoke),
        );
    app.world_mut().spawn((
        WeldOutput {
            id: OutputId::new(1),
        },
        PrimaryOutput,
        OutputGeometry::from_physical(UVec2::new(800, 600), 1.0),
    ));
    app.update();
    (app, sender)
}

fn prepare_launch(app: &mut App, sender: &mpsc::SyncSender<LaunchMessage>, options: LaunchOptions) {
    let (reply, response) = mpsc::sync_channel(1);
    sender
        .try_send(LaunchMessage::Prepare(Preparation {
            id: 17,
            options,
            deadline: Instant::now() + Duration::from_secs(3),
            reply,
        }))
        .ok()
        .expect("request");
    app.update();
    response.try_recv().expect("reply").expect("prepared");
}

fn map(app: &mut App, id: u64, launch: Option<u64>) -> Entity {
    let surface = weld_app::surface::SurfaceId::for_test(id);
    let client = app
        .world_mut()
        .spawn((
            ClientSource {
                id: surface.source(),
                provenance: ClientProvenance::Local,
            },
            ClientToplevel { surface },
            ClientWindowMetadata(
                ClientSurfaceMetadata::truncated("test".into(), "Test".into())
                    .with_local_launch_id(launch),
            ),
            MappedSurface {
                logical_size: Vec2::new(320., 240.),
                visual_size: Vec2::new(320., 240.),
                visual_offset: Vec2::ZERO,
                opaque: true,
                alpha_mode: Default::default(),
            },
        ))
        .id();
    app.update();
    app.world().get::<OccupiesWindow>(client).expect("window").0
}

#[test]
fn launch_destination_survives_workspace_switch_and_multiple_connections() {
    let (mut app, sender) = fixture();
    let original = app
        .world()
        .resource::<FocusedWorkspace>()
        .entity()
        .expect("workspace");
    let name = app
        .world()
        .get::<Workspace>(original)
        .expect("workspace")
        .name()
        .to_owned();
    prepare_launch(
        &mut app,
        &sender,
        LaunchOptions {
            workspace: Some("current".into()),
            ..Default::default()
        },
    );
    app.world_mut()
        .trigger(I3WorkspaceRequest::Switch(WorkspaceTarget::Name(
            "other".into(),
        )));
    app.update();
    let local = map(&mut app, 1, None);
    for id in [2, 3] {
        let window = map(&mut app, id, Some(17));
        let member = app
            .world()
            .get::<WorkspaceMember>(window)
            .expect("membership")
            .0;
        assert_eq!(
            app.world()
                .get::<Workspace>(member)
                .expect("workspace")
                .name(),
            name
        );
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(local)
        );
        assert_eq!(
            app.world().get::<WindowVisibility>(window),
            Some(&WindowVisibility::Hidden)
        );
    }
}

#[test]
fn headless_launch_stays_out_of_layout_and_dismiss_keeps_occupancy() {
    let (mut app, sender) = fixture();
    let local = map(&mut app, 1, None);
    prepare_launch(
        &mut app,
        &sender,
        LaunchOptions {
            headless: true,
            control: true,
            ..Default::default()
        },
    );
    let window = map(&mut app, 2, Some(17));
    assert!(app.world().get::<ManagedBy>(window).is_none());
    assert!(
        app.world()
            .get::<weld_window::WindowOccupant>(window)
            .is_some()
    );
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(local)
    );
    app.world_mut()
        .entity_mut(window)
        .remove::<WindowManagementHold>();
    app.update();
    assert!(app.world().get::<ManagedBy>(window).is_some());
    app.world_mut().trigger(weld_window::WindowCommand {
        window,
        kind: weld_window::WindowCommandKind::Close,
    });
    app.update();
    assert!(app.world().get::<ManagedBy>(window).is_none());
    assert!(
        app.world()
            .get::<weld_window::WindowOccupant>(window)
            .is_some()
    );
    assert!(app.world().get::<weld_tile::TileParent>(window).is_none());
    assert_eq!(
        app.world().get::<WindowVisibility>(window),
        Some(&WindowVisibility::Hidden)
    );
}

#[test]
fn headless_presentations_follow_peek_dismiss_and_output_availability() {
    for controlled_header in [false, true] {
        let (mut app, sender) = fixture();
        app.add_plugins((
            bevy::app::TaskPoolPlugin::default(),
            AssetPlugin::default(),
            ScenePlugin,
        ))
        .init_asset::<bevy::shader::Shader>()
        .insert_resource(Assets::<Image>::default())
        .insert_resource(UiScale(1.0))
        .add_message::<RequestRedraw>()
        .add_plugins((
            SurfacePlugin,
            weld_window_ui::WindowUiPlugin,
            weld_ssd::SsdPlugin,
        ));
        prepare_launch(
            &mut app,
            &sender,
            LaunchOptions {
                headless: true,
                control: true,
                ..Default::default()
            },
        );
        let window = map(&mut app, 2, Some(17));
        let occupant = app
            .world()
            .get::<WindowOccupant>(window)
            .expect("occupant")
            .entity();
        app.world_mut().entity_mut(occupant).insert(ClientDecorated);
        if controlled_header {
            app.world_mut()
                .entity_mut(window)
                .insert(WindowHeaderRequested);
        }
        app.update();
        let root = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("presentation")
            .entity();
        assert!(app.world().get::<WindowOutput>(window).is_none());
        assert_eq!(
            app.world()
                .get::<Node>(root)
                .expect("initial frame")
                .display,
            Display::None
        );

        for _ in 0..2 {
            app.world_mut()
                .entity_mut(window)
                .remove::<WindowManagementHold>();
            app.world_mut().trigger(I3WindowWorkspaceRequest {
                window,
                target: WorkspaceTarget::Current,
            });
            app.update();
            assert_eq!(
                app.world().get::<Node>(root).expect("peek frame").display,
                Display::Flex
            );
            let output = app.world().get::<WindowOutput>(window).expect("output").0;
            app.world_mut()
                .entity_mut(output)
                .remove::<OutputPosition>();
            app.update();
            assert_eq!(
                app.world()
                    .get::<Node>(root)
                    .expect("unavailable output")
                    .display,
                Display::None
            );
            app.world_mut()
                .entity_mut(output)
                .insert(OutputPosition::default());
            app.update();
            assert_eq!(
                app.world()
                    .get::<Node>(root)
                    .expect("restored output")
                    .display,
                Display::Flex
            );

            app.world_mut().trigger(weld_window::WindowCommand {
                window,
                kind: weld_window::WindowCommandKind::Close,
            });
            app.update();
            assert!(app.world().get::<WindowOutput>(window).is_none());
            assert!(app.world().get::<ManagedBy>(window).is_none());
            assert_eq!(
                app.world()
                    .get::<Node>(root)
                    .expect("dismissed frame")
                    .display,
                Display::None
            );
            assert_eq!(
                app.world()
                    .get::<WindowOccupant>(window)
                    .expect("retained app")
                    .entity(),
                occupant
            );
        }
    }
}

#[test]
fn rejected_launch_retires_its_policy_and_expired_preparation_never_grants() {
    let (mut app, sender) = fixture();
    prepare_launch(
        &mut app,
        &sender,
        LaunchOptions {
            control: true,
            ..Default::default()
        },
    );
    let permit = app.world().resource::<Launches>().0[&17]
        .permit
        .clone()
        .expect("permit");
    sender
        .try_send(LaunchMessage::Cancel(17))
        .ok()
        .expect("cancel");
    app.update();
    assert!(!permit.is_authorized());
    assert!(app.world().resource::<Launches>().0.is_empty());
    let (reply, response) = mpsc::sync_channel(1);
    sender
        .try_send(LaunchMessage::Prepare(Preparation {
            id: 18,
            options: LaunchOptions {
                control: true,
                ..Default::default()
            },
            deadline: Instant::now(),
            reply,
        }))
        .ok()
        .expect("expired request");
    app.update();
    assert!(response.try_recv().expect("reply").is_err());
    assert!(app.world().resource::<Launches>().0.is_empty());
}

#[test]
fn persistent_match_keeps_its_frame_and_unrelated_replacement_clears_launch_identity() {
    use weld_window::{
        WindowOccupant,
        persistence::{PersistenceAction, PersistenceMatch, WindowPersistenceRequest},
    };
    let (mut app, sender) = fixture();
    let slot = map(&mut app, 1, None);
    let workspace = app
        .world()
        .get::<WorkspaceMember>(slot)
        .expect("workspace")
        .0;
    app.world_mut().trigger(WindowPersistenceRequest {
        window: Some(slot),
        action: PersistenceAction::Enable(PersistenceMatch::Auto),
    });
    app.update();
    let client = app
        .world()
        .get::<WindowOccupant>(slot)
        .expect("client")
        .entity();
    app.world_mut().despawn(client);
    app.update();
    prepare_launch(
        &mut app,
        &sender,
        LaunchOptions {
            headless: true,
            control: true,
            owner: Some("test-owner".into()),
            ..Default::default()
        },
    );
    assert_eq!(map(&mut app, 2, Some(17)), slot);
    assert!(app.world().get::<WindowManagementHold>(slot).is_none());
    assert_eq!(
        app.world()
            .get::<WorkspaceMember>(slot)
            .expect("workspace")
            .0,
        workspace
    );
    assert!(app.world().get::<WindowLaunch>(slot).is_some());
    let client = app
        .world()
        .get::<WindowOccupant>(slot)
        .expect("client")
        .entity();
    app.world_mut().despawn(client);
    app.update();
    assert_eq!(map(&mut app, 3, None), slot);
    assert!(app.world().get::<WindowLaunch>(slot).is_none());
}
