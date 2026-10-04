use super::*;
use bevy::{
    asset::AssetApp,
    ecs::{message::Messages, system::RunSystemOnce},
    scene::ScenePlugin,
};
use tokio::sync::oneshot;
use weld_app::{client::ClientAdapterCommandQueue, surface::SurfacePlugin};
use weld_client::{ClientId, ClientSurfaceId};
use weld_hoist::{HoistPlugin, loopback_registration};
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
            ),
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

fn request(session: u64, window: u64) -> (DeviceAction, oneshot::Receiver<bool>) {
    let (answer, wait) = oneshot::channel();
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
    assert!(accepted.try_recv().expect("first"));
    assert!(!rejected.try_recv().expect("same-tick second family"));
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
    assert!(!rejected.try_recv().expect("occupied device"));
    assert!(!stolen.try_recv().expect("other device owns window"));
    assert!(allowed.try_recv().expect("independent window"));
    app.update();
    let expected: Vec<Entity> = app
        .world_mut()
        .query::<(Entity, &HoistSession)>()
        .iter(app.world())
        .filter_map(|(entity, session)| (session.endpoint() == endpoints[0]).then_some(entity))
        .collect();
    assert_eq!(expected.len(), 1);
    let (answer, mut released) = oneshot::channel();
    app.world_mut()
        .run_system_once_with(
            remote_actions,
            vec![DeviceAction::Release {
                session: SessionId(1),
                answer,
            }],
        )
        .expect("release");
    assert!(released.try_recv().expect("release accepted"));
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
    assert!(accepted.try_recv().expect("accepted"));
    assert!(
        !rejected
            .try_recv()
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
