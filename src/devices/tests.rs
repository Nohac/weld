use super::*;
use bevy::{
    asset::AssetApp,
    ecs::{message::Messages, system::RunSystemOnce},
    scene::ScenePlugin,
};
use weld_app::{client::ClientAdapterCommandQueue, surface::SurfacePlugin};
use weld_client::{ClientId, ClientSurfaceId};
use weld_hoist::{HoistPlugin, loopback_registration};
use weld_hoist_core::session::{Pending, reply_channel};
use weld_window::{OccupiesWindow, WindowId, WindowPlugin};

fn fixture() -> (App, [HoistEndpointId; 2]) {
    let mut app = App::new();
    app.add_plugins((
        bevy::app::TaskPoolPlugin::default(),
        AssetPlugin::default(),
        ScenePlugin,
    ));
    app.init_asset::<Shader>()
        .insert_resource(Assets::<Image>::default())
        .insert_resource(UiScale(1.0))
        .insert_resource(ClientAdapterCommandQueue::default())
        .add_message::<bevy::window::RequestRedraw>();
    let mut registry = HoistEndpointRegistry::default();
    let endpoints = [10, 11].map(|source| {
        let (_, endpoint) =
            loopback_registration(ClientSourceId::new(0), ClientSourceId::new(source));
        registry.register(endpoint).expect("endpoint")
    });
    app.insert_resource(registry)
        .add_plugins((
            SurfacePlugin,
            WindowPlugin,
            weld_window_ui::WindowUiPlugin,
            weld_ssd::SsdPlugin,
            weld_float::FloatPlugin,
            HoistPlugin,
        ))
        .insert_resource(Devices {
            sessions: DesktopSessions::new(
                IrohNotifier::new(|| Ok(())),
                IrohNotifier::new(|| Ok(())),
                VideoCodec::Av1,
            )
            .expect("session service"),
            arrivals: DesktopEndpoints::default(),
            endpoints: [(SessionId(1), endpoints[0]), (SessionId(2), endpoints[1])].into(),
        });
    (app, endpoints)
}

fn window(app: &mut App, id: u64, client: u64) -> Entity {
    let entity = app
        .world_mut()
        .spawn(ManagedWindow {
            id: WindowId::new(id),
        })
        .id();
    app.world_mut().spawn((
        ClientToplevel {
            surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), client), id),
        },
        MappedSurface {
            logical_size: Vec2::new(320.0, 240.0),
            visual_offset: Vec2::ZERO,
            visual_size: Vec2::new(320.0, 240.0),
            opaque: true,
            alpha_mode: Default::default(),
        },
        OccupiesWindow(entity),
    ));
    entity
}

fn request(session: u64, window: u64) -> (DeviceAction, Pending<bool>) {
    let (answer, wait) = reply_channel();
    (
        DeviceAction::Hoist {
            session: SessionId(session),
            window,
            answer,
        },
        wait,
    )
}

#[test]
fn selection_and_release_are_scoped_to_the_approved_endpoint() {
    let (mut app, endpoints) = fixture();
    window(&mut app, 1, 1);
    window(&mut app, 2, 2);
    let (first, mut accepted) = request(1, 1);
    let (second, mut rejected) = request(1, 2);
    app.world_mut()
        .run_system_once_with(remote_actions, vec![first, second])
        .expect("actions");
    assert!(accepted.poll().expect("response ready").expect("first"));
    assert!(
        !rejected
            .poll()
            .expect("response ready")
            .expect("same-tick second family")
    );
    app.update();
    assert_eq!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .count(),
        1
    );

    let (second, mut rejected) = request(1, 2);
    let (steal, mut stolen) = request(2, 1);
    let (own, mut allowed) = request(2, 2);
    app.world_mut()
        .run_system_once_with(remote_actions, vec![second, steal, own])
        .expect("actions");
    assert!(
        !rejected
            .poll()
            .expect("response ready")
            .expect("occupied device")
    );
    assert!(
        !stolen
            .poll()
            .expect("response ready")
            .expect("other device owns window")
    );
    assert!(
        allowed
            .poll()
            .expect("response ready")
            .expect("independent window")
    );
    app.update();
    let expected: Vec<Entity> = app
        .world_mut()
        .query::<(Entity, &HoistSession)>()
        .iter(app.world())
        .filter_map(|(entity, session)| (session.endpoint() == endpoints[0]).then_some(entity))
        .collect();
    assert_eq!(expected.len(), 1);
    let (answer, mut released) = reply_channel();
    app.world_mut()
        .run_system_once_with(
            remote_actions,
            vec![DeviceAction::Release {
                session: SessionId(1),
                answer,
            }],
        )
        .expect("release");
    assert!(
        released
            .poll()
            .expect("response ready")
            .expect("release accepted")
    );
    let actual: Vec<Entity> = app
        .world_mut()
        .resource_mut::<Messages<ReclaimHoist>>()
        .drain()
        .map(|request| request.session)
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn cancelled_requests_never_hoist_and_one_client_cannot_be_split_between_devices() {
    let (mut app, _) = fixture();
    window(&mut app, 1, 1);
    window(&mut app, 2, 1);
    let (cancelled, wait) = request(1, 1);
    drop(wait);
    let (first, mut accepted) = request(1, 1);
    let (second, mut rejected) = request(2, 2);
    app.world_mut()
        .run_system_once_with(remote_actions, vec![cancelled, first, second])
        .expect("actions");
    assert!(accepted.poll().expect("response ready").expect("accepted"));
    assert!(
        !rejected
            .poll()
            .expect("response ready")
            .expect("same client cannot cross endpoints")
    );
    assert_eq!(
        app.world_mut()
            .resource_mut::<Messages<HoistWindow>>()
            .drain()
            .count(),
        1
    );
}

#[test]
fn catalogue_change_detection_tracks_lifecycle_without_republishing_idle_frames() {
    #[derive(Resource, Default)]
    struct Changes(usize);
    let mut app = App::new();
    app.init_resource::<Changes>().add_systems(
        Update,
        |mut changes: CatalogueChanges, mut count: ResMut<Changes>| {
            if changes.changed() {
                count.0 += 1;
            }
        },
    );
    let entity = app
        .world_mut()
        .spawn(ManagedWindow {
            id: WindowId::new(1),
        })
        .id();
    app.update();
    assert_eq!(app.world().resource::<Changes>().0, 1);
    app.update();
    app.update();
    assert_eq!(app.world().resource::<Changes>().0, 1);
    app.world_mut()
        .entity_mut(entity)
        .insert(ClientWindowMetadata(
            weld_client::ClientSurfaceMetadata::new("app".into(), "Title".into())
                .expect("metadata"),
        ));
    app.update();
    assert_eq!(app.world().resource::<Changes>().0, 2);
    app.world_mut()
        .entity_mut(entity)
        .remove::<ClientWindowMetadata>();
    app.update();
    assert_eq!(app.world().resource::<Changes>().0, 3);
    app.world_mut().despawn(entity);
    app.update();
    assert_eq!(app.world().resource::<Changes>().0, 4);
    app.update();
    assert_eq!(app.world().resource::<Changes>().0, 4);
}
