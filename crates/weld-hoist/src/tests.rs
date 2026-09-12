use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use bevy::{
    app::App,
    asset::{AssetApp, AssetPlugin, Assets},
    ecs::message::Messages,
    image::Image,
    math::{UVec2, Vec2},
    scene::ScenePlugin,
    ui::UiScale,
    window::RequestRedraw,
};
use weld_app::{
    client::ClientAdapterCommandQueue,
    output::{OutputGeometry, OutputId, OutputPosition, PrimaryOutput, WeldOutput},
    surface::{
        ClientProvenance, ClientSource, ClientToplevel, HostSurfaceEvent, HostSurfaceEventKind,
        SurfaceAction, SurfaceBufferContent, SurfaceBufferUpdate, SurfaceCommitRevisions,
        SurfaceContentView, SurfaceId, SurfaceLayerId, SurfaceLayerPlacement, SurfacePlugin,
        SurfaceTreeSnapshot, SurfaceWindowGeometry, ToplevelInteractionRequestKind,
        WindowDecoration, enqueue_surface_event, register_client_source, take_surface_actions,
    },
};
use weld_client::{
    ClientAdapterCommandEnvelope, ClientId, ClientSourceDescriptor, ClientSourceId,
    ClientSurfaceRole, ToplevelState,
};
use weld_float::FloatPlugin;
use weld_ssd::SsdPlugin;
use weld_window::{
    ClientResizeState, OccupiesWindow, PrimaryWindowPresentation, WindowAdmissionHold,
    WindowGeometry, WindowInteractionKind, WindowInteractionSession, WindowOccupant, WindowOutput,
    WindowPlugin,
};
use weld_window_ui::WindowUiPlugin;

use crate::{
    DismissHoistTombstone, HoistDetached, HoistEndpoint, HoistEndpointRegistry, HoistFamilyId,
    HoistPlaceholder, HoistPlugin, HoistSession, HoistSessionId, HoistWindow, ReclaimHoist,
    SessionState, loopback_registration,
};

const LOCAL_SOURCE: ClientSourceId = ClientSourceId::new(0);
const LOOPBACK_SOURCE: ClientSourceId = ClientSourceId::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EndpointCall {
    Map(SurfaceId),
    Unmap(SurfaceId),
}

#[derive(Clone)]
struct RecordingEndpoint {
    inner: weld_hoist_core::LoopbackEndpoint,
    available: Arc<AtomicBool>,
    calls: Arc<Mutex<Vec<EndpointCall>>>,
}

impl RecordingEndpoint {
    fn new(inner: weld_hoist_core::LoopbackEndpoint) -> Self {
        Self {
            inner,
            available: Arc::new(AtomicBool::new(true)),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::Relaxed);
    }

    fn take_calls(&self) -> Vec<EndpointCall> {
        std::mem::take(&mut *self.calls.lock().expect("endpoint call recorder"))
    }
}

impl HoistEndpoint for RecordingEndpoint {
    fn is_available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }

    fn has_local_receiver(&self) -> bool {
        self.inner.has_local_receiver()
    }

    fn destination(&self, source: SurfaceId) -> SurfaceId {
        self.inner.destination(source)
    }

    fn map(&self, session: HoistSessionId, source: SurfaceId) -> ClientAdapterCommandEnvelope {
        self.calls
            .lock()
            .expect("endpoint call recorder")
            .push(EndpointCall::Map(source));
        self.inner.map(session, source)
    }

    fn unmap(&self, source: SurfaceId) -> ClientAdapterCommandEnvelope {
        self.calls
            .lock()
            .expect("endpoint call recorder")
            .push(EndpointCall::Unmap(source));
        self.inner.unmap(source)
    }
}

fn test_app() -> (App, weld_hoist_core::LoopbackEndpoint) {
    let (_, endpoint) = loopback_registration(LOCAL_SOURCE, LOOPBACK_SOURCE);
    let mut app = App::new();
    app.add_plugins((
        bevy::app::TaskPoolPlugin::default(),
        AssetPlugin::default(),
        ScenePlugin,
    ));
    app.init_asset::<bevy::shader::Shader>()
        .insert_resource(Assets::<Image>::default())
        .insert_resource(UiScale(1.0))
        .insert_resource(ClientAdapterCommandQueue::default())
        .insert_resource(HoistEndpointRegistry::with_default(endpoint))
        .add_message::<RequestRedraw>()
        .add_plugins((
            SurfacePlugin,
            WindowPlugin,
            WindowUiPlugin,
            SsdPlugin,
            FloatPlugin,
            HoistPlugin,
        ));
    assert!(register_client_source(
        app.world_mut(),
        ClientSourceDescriptor::new(LOOPBACK_SOURCE, ClientProvenance::Relocated),
    ));
    app.world_mut().spawn((
        WeldOutput {
            id: OutputId::new(1),
        },
        OutputGeometry::from_physical(UVec2::new(1_000, 800), 1.0),
        OutputPosition::default(),
        PrimaryOutput,
    ));
    (app, endpoint)
}

fn surface(source: ClientSourceId, local: u64) -> SurfaceId {
    surface_for_client(source, 1, local)
}

fn surface_for_client(source: ClientSourceId, client_local: u64, local: u64) -> SurfaceId {
    SurfaceId::new(ClientId::new(source, client_local), local)
}

fn map_surface(
    app: &mut App,
    surface: SurfaceId,
    parent: Option<SurfaceId>,
    decoration: WindowDecoration,
) -> bevy::ecs::entity::Entity {
    enqueue_surface_event(
        app.world_mut(),
        HostSurfaceEvent {
            surface,
            kind: HostSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                parent,
                decoration,
            })),
        },
    );
    enqueue_surface_event(app.world_mut(), commit_event(surface, UVec2::new(320, 240)));
    app.update();
    app.world_mut()
        .query::<(bevy::ecs::entity::Entity, &ClientToplevel)>()
        .iter(app.world())
        .find_map(|(entity, toplevel)| (toplevel.surface == surface).then_some(entity))
        .expect("mapped client entity")
}

fn commit_event(surface: SurfaceId, size: UVec2) -> HostSurfaceEvent {
    let view = SurfaceContentView {
        source_x: 0.0,
        source_y: 0.0,
        source_width: size.x as f32,
        source_height: size.y as f32,
        logical_width: size.x as f32,
        logical_height: size.y as f32,
    };
    HostSurfaceEvent {
        surface,
        kind: HostSurfaceEventKind::Commit(SurfaceTreeSnapshot {
            client_mapped: true,
            alpha_mode: Default::default(),
            root: Some(SurfaceLayerPlacement {
                layer: SurfaceLayerId::new(1),
                position: Vec2::ZERO,
                view,
            }),
            window_geometry: Some(SurfaceWindowGeometry {
                origin: Vec2::ZERO,
                view,
            }),
            overlays: Vec::new(),
            inputs: Vec::new(),
            buffers: vec![SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(1),
                width: size.x,
                height: size.y,
                content: SurfaceBufferContent::Pixels(vec![
                    0;
                    size.x as usize * size.y as usize * 4
                ]),
                opaque: true,
            }],
        }),
    }
}

fn commit_surface(app: &mut App, surface: SurfaceId, size: UVec2) {
    enqueue_surface_event(app.world_mut(), commit_event(surface, size));
}

fn set_parent(app: &mut App, surface: SurfaceId, parent: Option<SurfaceId>) {
    enqueue_surface_event(
        app.world_mut(),
        HostSurfaceEvent {
            surface,
            kind: HostSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                parent,
                decoration: WindowDecoration::ServerSide,
            })),
        },
    );
}

fn unmap_surface(app: &mut App, surface: SurfaceId) {
    enqueue_surface_event(
        app.world_mut(),
        HostSurfaceEvent {
            surface,
            kind: HostSurfaceEventKind::Commit(SurfaceTreeSnapshot {
                client_mapped: false,
                alpha_mode: Default::default(),
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: Vec::new(),
            }),
        },
    );
}

fn window_for_client(
    app: &mut App,
    client: bevy::ecs::entity::Entity,
) -> bevy::ecs::entity::Entity {
    app.world()
        .get::<OccupiesWindow>(client)
        .map(|occupancy| occupancy.0)
        .expect("client should occupy a window")
}

fn begin_hoist(
    app: &mut App,
    endpoint: weld_hoist_core::LoopbackEndpoint,
    source_window: bevy::ecs::entity::Entity,
    source_surface: SurfaceId,
    decoration: WindowDecoration,
) -> (bevy::ecs::entity::Entity, bevy::ecs::entity::Entity) {
    app.world_mut().write_message(HoistWindow {
        window: source_window,
    });
    app.update();
    let session = app
        .world_mut()
        .query::<(bevy::ecs::entity::Entity, &HoistSession)>()
        .iter(app.world())
        .find_map(|(entity, session)| (session.surface() == source_surface).then_some(entity))
        .expect("hoist session");
    let destination = endpoint.destination(source_surface);
    let _destination_client = map_surface(app, destination, None, decoration);
    app.update();
    let receiver = app
        .world()
        .get::<HoistSession>(session)
        .and_then(HoistSession::receiver)
        .expect("relocated receiver window");
    (session, receiver)
}

fn destroy_surface(app: &mut App, surface: SurfaceId) {
    enqueue_surface_event(
        app.world_mut(),
        HostSurfaceEvent {
            surface,
            kind: HostSurfaceEventKind::Destroyed,
        },
    );
}

#[test]
fn hoist_uses_an_independent_relocated_client_and_restores_after_ordered_unmap() {
    let (mut app, endpoint) = test_app();
    let source_surface = surface(LOCAL_SOURCE, 1);
    let source_client = map_surface(&mut app, source_surface, None, WindowDecoration::ServerSide);
    let source_window = window_for_client(&mut app, source_client);
    app.world_mut().write_message(HoistWindow {
        window: source_window,
    });
    app.update();

    assert!(
        app.world()
            .get::<WindowAdmissionHold>(source_client)
            .is_some()
    );
    assert!(app.world().get::<OccupiesWindow>(source_client).is_none());
    assert!(app.world().get::<HoistPlaceholder>(source_window).is_some());
    let (session_entity, destination) = app
        .world_mut()
        .query::<(bevy::ecs::entity::Entity, &HoistSession)>()
        .single(app.world())
        .map(|(entity, session)| (entity, session.destination()))
        .expect("hoist session");
    assert_eq!(destination, endpoint.destination(source_surface));

    let destination_client = map_surface(&mut app, destination, None, WindowDecoration::ServerSide);
    app.update();
    let receiver = app
        .world()
        .get::<HoistSession>(session_entity)
        .and_then(HoistSession::receiver)
        .expect("relocated receiver window");
    assert_ne!(destination_client, source_client);
    assert_ne!(receiver, source_window);
    assert_eq!(
        app.world()
            .get::<ClientSource>(destination_client)
            .map(|source| source.provenance),
        Some(ClientProvenance::Relocated)
    );
    assert!(app.world().get::<WindowOccupant>(receiver).is_some());
    assert!(
        app.world()
            .get::<PrimaryWindowPresentation>(receiver)
            .is_some()
    );

    app.world_mut().write_message(ReclaimHoist {
        session: session_entity,
    });
    app.update();
    if let Some(mut session) = app.world_mut().get_mut::<HoistSession>(session_entity)
        && let SessionState::Reclaiming {
            scope,
            target_size,
            resize_required,
            resize_request_observed,
            remote_after_revision,
            ..
        } = session.state
    {
        session.state = SessionState::Reclaiming {
            scope,
            target_size,
            resize_required,
            resize_request_observed,
            remote_after_revision,
            deadline: Instant::now() - Duration::from_millis(1),
        };
    }
    app.update();
    assert!(matches!(
        app.world()
            .get::<HoistSession>(session_entity)
            .map(|session| session.state),
        Some(SessionState::Unmapping)
    ));
    assert!(app.world().get::<OccupiesWindow>(source_client).is_none());

    enqueue_surface_event(
        app.world_mut(),
        HostSurfaceEvent {
            surface: destination,
            kind: HostSurfaceEventKind::Destroyed,
        },
    );
    app.update();
    app.update();
    assert_eq!(
        app.world()
            .get::<OccupiesWindow>(source_client)
            .map(|occupancy| occupancy.0),
        Some(source_window)
    );
    assert!(
        app.world()
            .get::<WindowAdmissionHold>(source_client)
            .is_none()
    );
    assert!(app.world().get::<HoistPlaceholder>(source_window).is_none());
    assert!(!app.world().entities().contains(session_entity));
}

#[test]
fn initial_family_members_preserve_slots_and_later_members_follow_without_placeholders() {
    let (mut app, _) = test_app();
    let root_surface = surface(LOCAL_SOURCE, 10);
    let child_surface = surface(LOCAL_SOURCE, 11);
    let root_client = map_surface(&mut app, root_surface, None, WindowDecoration::ServerSide);
    let child_client = map_surface(
        &mut app,
        child_surface,
        Some(root_surface),
        WindowDecoration::ServerSide,
    );
    let root_window = window_for_client(&mut app, root_client);
    let child_window = window_for_client(&mut app, child_client);
    app.world_mut().write_message(HoistWindow {
        window: root_window,
    });
    app.update();

    assert!(app.world().get::<HoistPlaceholder>(root_window).is_some());
    assert!(app.world().get::<HoistPlaceholder>(child_window).is_some());
    assert_eq!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .count(),
        2
    );

    let later_surface = surface(LOCAL_SOURCE, 12);
    let later_client = map_surface(
        &mut app,
        later_surface,
        Some(root_surface),
        WindowDecoration::ServerSide,
    );
    let later_window = window_for_client(&mut app, later_client);
    app.update();

    assert!(
        app.world()
            .get::<WindowAdmissionHold>(later_client)
            .is_some()
    );
    assert!(!app.world().entities().contains(later_window));
    assert_eq!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .count(),
        3
    );

    let later_session = app
        .world_mut()
        .query::<(bevy::ecs::entity::Entity, &HoistSession)>()
        .iter(app.world())
        .find_map(|(entity, session)| (session.surface() == later_surface).then_some(entity))
        .expect("followed session");
    destroy_surface(&mut app, later_surface);
    app.update();
    assert!(!app.world().entities().contains(later_session));
}

#[test]
fn independent_same_client_toplevels_follow_without_absorbing_other_clients() {
    let (mut app, _) = test_app();
    let root_surface = surface_for_client(LOCAL_SOURCE, 10, 60);
    let peer_surface = surface_for_client(LOCAL_SOURCE, 10, 61);
    let outsider_surface = surface_for_client(LOCAL_SOURCE, 11, 62);
    let root_client = map_surface(&mut app, root_surface, None, WindowDecoration::ServerSide);
    let peer_client = map_surface(&mut app, peer_surface, None, WindowDecoration::ServerSide);
    let outsider_client = map_surface(
        &mut app,
        outsider_surface,
        None,
        WindowDecoration::ServerSide,
    );
    let root_window = window_for_client(&mut app, root_client);
    let peer_window = window_for_client(&mut app, peer_client);
    let outsider_window = window_for_client(&mut app, outsider_client);

    app.world_mut().write_message(HoistWindow {
        window: root_window,
    });
    app.update();

    assert!(app.world().get::<HoistPlaceholder>(root_window).is_some());
    assert!(app.world().get::<HoistPlaceholder>(peer_window).is_some());
    assert!(
        app.world()
            .get::<HoistPlaceholder>(outsider_window)
            .is_none()
    );
    assert!(app.world().get::<OccupiesWindow>(outsider_client).is_some());
    assert_eq!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .count(),
        2
    );

    let later_surface = surface_for_client(LOCAL_SOURCE, 10, 63);
    let later_client = map_surface(&mut app, later_surface, None, WindowDecoration::ServerSide);
    let later_window = window_for_client(&mut app, later_client);
    app.update();

    assert!(
        app.world()
            .get::<WindowAdmissionHold>(later_client)
            .is_some()
    );
    assert!(!app.world().entities().contains(later_window));
    assert_eq!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .count(),
        3
    );
}

#[test]
fn closed_tombstone_does_not_block_following_a_replacement_peer() {
    let (mut app, _) = test_app();
    let root_surface = surface_for_client(LOCAL_SOURCE, 12, 64);
    let closed_surface = surface_for_client(LOCAL_SOURCE, 12, 65);
    let root_client = map_surface(&mut app, root_surface, None, WindowDecoration::ServerSide);
    let closed_client = map_surface(&mut app, closed_surface, None, WindowDecoration::ServerSide);
    let root_window = window_for_client(&mut app, root_client);
    let closed_window = window_for_client(&mut app, closed_client);
    app.world_mut().write_message(HoistWindow {
        window: root_window,
    });
    app.update();
    let closed_session = app
        .world_mut()
        .query::<(bevy::ecs::entity::Entity, &HoistSession)>()
        .iter(app.world())
        .find_map(|(entity, session)| (session.surface() == closed_surface).then_some(entity))
        .expect("closed member session");
    destroy_surface(&mut app, closed_surface);
    app.update();
    assert_eq!(
        app.world()
            .get::<HoistPlaceholder>(closed_window)
            .map(|placeholder| placeholder.state),
        Some(crate::HoistPlaceholderState::Closed)
    );

    let later_surface = surface_for_client(LOCAL_SOURCE, 12, 66);
    let later_client = map_surface(&mut app, later_surface, None, WindowDecoration::ServerSide);
    let later_window = window_for_client(&mut app, later_client);
    app.update();
    assert!(
        app.world()
            .get::<WindowAdmissionHold>(later_client)
            .is_some()
    );
    assert!(!app.world().entities().contains(later_window));
    let replacement_session = app
        .world_mut()
        .query::<(bevy::ecs::entity::Entity, &HoistSession)>()
        .iter(app.world())
        .find_map(|(entity, session)| (session.surface() == later_surface).then_some(entity))
        .expect("replacement peer session");
    assert!(app.world().entities().contains(closed_session));
    assert!(app.world().entities().contains(closed_window));

    app.world_mut().write_message(DismissHoistTombstone {
        session: closed_session,
    });
    app.update();
    assert!(
        app.world()
            .get::<WindowAdmissionHold>(later_client)
            .is_some()
    );
    assert!(app.world().entities().contains(replacement_session));
}

#[test]
fn reclaim_is_atomic_for_independent_same_client_peers_and_allows_a_new_family() {
    let (mut app, endpoint) = test_app();
    let root_surface = surface_for_client(LOCAL_SOURCE, 13, 67);
    let peer_surface = surface_for_client(LOCAL_SOURCE, 13, 68);
    let root_client = map_surface(&mut app, root_surface, None, WindowDecoration::ServerSide);
    let peer_client = map_surface(&mut app, peer_surface, None, WindowDecoration::ServerSide);
    let root_window = window_for_client(&mut app, root_client);
    let peer_window = window_for_client(&mut app, peer_client);
    app.world_mut().write_message(HoistWindow {
        window: root_window,
    });
    app.update();

    let root_destination = endpoint.destination(root_surface);
    let peer_destination = endpoint.destination(peer_surface);
    let _root_destination_client = map_surface(
        &mut app,
        root_destination,
        None,
        WindowDecoration::ServerSide,
    );
    let _peer_destination_client = map_surface(
        &mut app,
        peer_destination,
        None,
        WindowDecoration::ServerSide,
    );
    app.update();
    let sessions = app
        .world_mut()
        .query::<(bevy::ecs::entity::Entity, &HoistSession)>()
        .iter(app.world())
        .map(|(entity, session)| (entity, session.family()))
        .collect::<Vec<_>>();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].1, sessions[1].1);

    app.world_mut().write_message(ReclaimHoist {
        session: sessions[0].0,
    });
    app.update();
    let blocked_surface = surface_for_client(LOCAL_SOURCE, 13, 69);
    let blocked_client = map_surface(
        &mut app,
        blocked_surface,
        None,
        WindowDecoration::ServerSide,
    );
    let blocked_window = window_for_client(&mut app, blocked_client);
    app.update();
    assert!(app.world().get::<OccupiesWindow>(blocked_client).is_some());
    assert!(app.world().entities().contains(blocked_window));
    for (entity, _) in &sessions {
        if let Some(mut session) = app.world_mut().get_mut::<HoistSession>(*entity)
            && let SessionState::Reclaiming {
                scope,
                target_size,
                resize_required,
                resize_request_observed,
                remote_after_revision,
                ..
            } = session.state
        {
            session.state = SessionState::Reclaiming {
                scope,
                target_size,
                resize_required,
                resize_request_observed,
                remote_after_revision,
                deadline: Instant::now() - Duration::from_millis(1),
            };
        }
    }
    app.update();
    assert!(sessions.iter().all(|(entity, _)| {
        matches!(
            app.world()
                .get::<HoistSession>(*entity)
                .map(|session| session.state),
            Some(SessionState::Unmapping)
        )
    }));

    destroy_surface(&mut app, root_destination);
    destroy_surface(&mut app, peer_destination);
    app.update();
    app.update();
    assert_eq!(
        app.world()
            .get::<OccupiesWindow>(root_client)
            .map(|occupancy| occupancy.0),
        Some(root_window)
    );
    assert_eq!(
        app.world()
            .get::<OccupiesWindow>(peer_client)
            .map(|occupancy| occupancy.0),
        Some(peer_window)
    );

    app.world_mut().write_message(HoistWindow {
        window: root_window,
    });
    app.update();
    assert!(app.world().get::<HoistPlaceholder>(root_window).is_some());
    assert!(app.world().get::<HoistPlaceholder>(peer_window).is_some());
    assert!(
        app.world()
            .get::<HoistPlaceholder>(blocked_window)
            .is_some()
    );
}

#[test]
fn protocol_unmap_ends_the_relocation_without_marking_the_source_closed() {
    let (mut app, _) = test_app();
    let source_surface = surface(LOCAL_SOURCE, 20);
    let source_client = map_surface(&mut app, source_surface, None, WindowDecoration::ServerSide);
    let source_window = window_for_client(&mut app, source_client);
    app.world_mut().write_message(HoistWindow {
        window: source_window,
    });
    app.update();

    let (session_entity, destination) = app
        .world_mut()
        .query::<(bevy::ecs::entity::Entity, &HoistSession)>()
        .single(app.world())
        .map(|(entity, session)| (entity, session.destination()))
        .expect("hoist session");
    let _destination_client =
        map_surface(&mut app, destination, None, WindowDecoration::ServerSide);
    app.update();

    unmap_surface(&mut app, source_surface);
    app.update();
    assert!(matches!(
        app.world()
            .get::<HoistSession>(session_entity)
            .map(|session| session.state),
        Some(SessionState::Unmapping)
    ));
    assert_eq!(
        app.world()
            .get::<HoistPlaceholder>(source_window)
            .map(|placeholder| placeholder.state),
        Some(crate::HoistPlaceholderState::Live)
    );

    enqueue_surface_event(
        app.world_mut(),
        HostSurfaceEvent {
            surface: destination,
            kind: HostSurfaceEventKind::Destroyed,
        },
    );
    app.update();
    app.update();

    assert!(!app.world().entities().contains(session_entity));
    assert_eq!(
        app.world()
            .get::<OccupiesWindow>(source_client)
            .map(|occupancy| occupancy.0),
        Some(source_window)
    );
    app.update();
    assert_eq!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .count(),
        0
    );
    assert!(app.world().get::<HoistPlaceholder>(source_window).is_none());
}

#[test]
fn reclaim_waits_for_the_placeholder_sized_client_commit() {
    let (mut app, endpoint) = test_app();
    let source_surface = surface(LOCAL_SOURCE, 30);
    let source_client = map_surface(&mut app, source_surface, None, WindowDecoration::ServerSide);
    let source_window = window_for_client(&mut app, source_client);
    let (session, receiver) = begin_hoist(
        &mut app,
        endpoint,
        source_window,
        source_surface,
        WindowDecoration::ServerSide,
    );
    let destination = endpoint.destination(source_surface);
    take_surface_actions(app.world_mut());

    app.world_mut()
        .get_mut::<WindowGeometry>(receiver)
        .expect("receiver geometry")
        .size += Vec2::new(80.0, 40.0);
    app.update();
    let resize_actions = take_surface_actions(app.world_mut());
    let expanded_size = resize_actions
        .iter()
        .find_map(|action| match action {
            SurfaceAction::Resize {
                surface,
                logical_size,
                ..
            } if *surface == destination => Some(*logical_size),
            _ => None,
        })
        .expect("receiver resize should configure the relocated client");
    commit_surface(&mut app, destination, expanded_size);
    app.update();
    take_surface_actions(app.world_mut());

    app.world_mut().write_message(ReclaimHoist { session });
    app.update();
    assert!(matches!(
        app.world()
            .get::<HoistSession>(session)
            .map(|session| session.state),
        Some(SessionState::Reclaiming { .. })
    ));
    let reclaim_actions = take_surface_actions(app.world_mut());
    assert!(
        reclaim_actions.iter().any(|action| {
            matches!(
                action,
                SurfaceAction::Resize {
                    surface,
                    logical_size: UVec2 { x: 320, y: 240 },
                    resizing: false,
                } if *surface == destination
            )
        }),
        "unexpected reclaim actions: {reclaim_actions:?}"
    );

    app.update();
    assert!(matches!(
        app.world()
            .get::<HoistSession>(session)
            .map(|session| session.state),
        Some(SessionState::Reclaiming { .. })
    ));
    commit_surface(&mut app, destination, UVec2::new(320, 240));
    app.update();
    assert!(matches!(
        app.world()
            .get::<HoistSession>(session)
            .map(|session| session.state),
        Some(SessionState::Unmapping)
    ));

    destroy_surface(&mut app, destination);
    app.update();
    app.update();
    assert_eq!(
        app.world()
            .get::<OccupiesWindow>(source_client)
            .map(|occupancy| occupancy.0),
        Some(source_window)
    );
    app.update();
    assert_eq!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .count(),
        0
    );
}

#[test]
fn reclaim_timeout_proceeds_without_a_client_commit() {
    let (mut app, endpoint) = test_app();
    let source_surface = surface(LOCAL_SOURCE, 31);
    let source_client = map_surface(&mut app, source_surface, None, WindowDecoration::ServerSide);
    let source_window = window_for_client(&mut app, source_client);
    let (session, receiver) = begin_hoist(
        &mut app,
        endpoint,
        source_window,
        source_surface,
        WindowDecoration::ServerSide,
    );
    app.world_mut()
        .get_mut::<WindowGeometry>(receiver)
        .expect("receiver geometry")
        .size += Vec2::new(80.0, 40.0);
    app.update();
    app.world_mut().write_message(ReclaimHoist { session });
    app.update();
    if let Some(mut session) = app.world_mut().get_mut::<HoistSession>(session)
        && let SessionState::Reclaiming {
            scope,
            target_size,
            resize_required,
            resize_request_observed,
            remote_after_revision,
            ..
        } = session.state
    {
        session.state = SessionState::Reclaiming {
            scope,
            target_size,
            resize_required,
            resize_request_observed,
            remote_after_revision,
            deadline: Instant::now() - Duration::from_millis(1),
        };
    }

    app.update();
    assert!(matches!(
        app.world()
            .get::<HoistSession>(session)
            .map(|session| session.state),
        Some(SessionState::Unmapping)
    ));
    destroy_surface(&mut app, endpoint.destination(source_surface));
    app.update();
    app.update();
    assert_eq!(
        app.world()
            .get::<OccupiesWindow>(source_client)
            .map(|occupancy| occupancy.0),
        Some(source_window)
    );
}

#[test]
fn receiver_loss_uses_ordered_unmap_before_restoring_the_source() {
    let (mut app, endpoint) = test_app();
    let source_surface = surface(LOCAL_SOURCE, 32);
    let source_client = map_surface(&mut app, source_surface, None, WindowDecoration::ServerSide);
    let source_window = window_for_client(&mut app, source_client);
    let (session, receiver) = begin_hoist(
        &mut app,
        endpoint,
        source_window,
        source_surface,
        WindowDecoration::ServerSide,
    );

    app.world_mut().despawn(receiver);
    app.update();
    assert!(matches!(
        app.world()
            .get::<HoistSession>(session)
            .map(|session| session.state),
        Some(SessionState::Unmapping)
    ));
    assert!(app.world().get::<OccupiesWindow>(source_client).is_none());

    destroy_surface(&mut app, endpoint.destination(source_surface));
    app.update();
    app.update();
    assert_eq!(
        app.world()
            .get::<OccupiesWindow>(source_client)
            .map(|occupancy| occupancy.0),
        Some(source_window)
    );
    app.update();
    assert_eq!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .count(),
        0
    );
}

#[test]
fn remote_admission_requests_its_size_without_a_preserved_source_window() {
    let (mut app, _) = test_app();
    let destination = surface(LOOPBACK_SOURCE, 99);
    let client = map_surface(&mut app, destination, None, WindowDecoration::ServerSide);
    let window = window_for_client(&mut app, client);
    app.update();
    assert_eq!(
        take_surface_actions(app.world_mut())
            .into_iter()
            .filter(|action| matches!(action, SurfaceAction::Resize { .. }))
            .collect::<Vec<_>>(),
        vec![SurfaceAction::Resize {
            surface: destination,
            logical_size: UVec2::new(320, 240),
            resizing: false,
        }]
    );
    let geometry = *app.world().get::<WindowGeometry>(window).expect("geometry");
    let after_revision = app
        .world()
        .get::<ClientResizeState>(window)
        .expect("resize state")
        .pending_after_revision(destination)
        .expect("initial configure should await a commit");

    // A client can commit a different size while processing startup configures
    // or enforcing constraints. It must not silently change the window's choice
    // or cause us to resend the same configure on every reconciliation pass.
    commit_surface(&mut app, destination, UVec2::new(160, 120));
    app.update();
    app.update();
    assert!(
        app.world()
            .resource::<SurfaceCommitRevisions>()
            .revision(destination)
            > after_revision
    );
    assert_eq!(app.world().get::<WindowGeometry>(window), Some(&geometry));
    assert_eq!(
        app.world()
            .get::<ClientResizeState>(window)
            .expect("resize state")
            .pending_after_revision(destination),
        None
    );
    assert!(
        take_surface_actions(app.world_mut())
            .iter()
            .all(|action| !matches!(action, SurfaceAction::Resize { .. }))
    );
}

#[test]
fn relocated_receiver_owns_output_and_resize_policy() {
    let (mut app, endpoint) = test_app();
    let secondary = app
        .world_mut()
        .spawn((
            WeldOutput {
                id: OutputId::new(2),
            },
            OutputGeometry::from_physical(UVec2::new(1_000, 800), 1.5),
            OutputPosition(Vec2::new(1_000.0, 0.0)),
        ))
        .id();
    let source_surface = surface(LOCAL_SOURCE, 33);
    let source_client = map_surface(&mut app, source_surface, None, WindowDecoration::ServerSide);
    let source_window = window_for_client(&mut app, source_client);
    let (_, receiver) = begin_hoist(
        &mut app,
        endpoint,
        source_window,
        source_surface,
        WindowDecoration::ServerSide,
    );
    let destination = endpoint.destination(source_surface);
    take_surface_actions(app.world_mut());

    app.world_mut()
        .entity_mut(receiver)
        .insert(WindowOutput(secondary));
    app.world_mut()
        .get_mut::<WindowGeometry>(receiver)
        .expect("receiver geometry")
        .position = Vec2::new(100.0, 100.0);
    app.update();
    assert!(take_surface_actions(app.world_mut()).iter().any(|action| {
        matches!(
            action,
            SurfaceAction::SetOutputs {
                surface,
                outputs,
                preferred: Some(preferred),
                ..
            } if *surface == destination
                && outputs == &[OutputId::new(2)]
                && *preferred == OutputId::new(2)
        )
    }));

    app.world_mut()
        .get_mut::<WindowGeometry>(source_window)
        .expect("placeholder geometry")
        .position += Vec2::new(40.0, 30.0);
    app.update();
    assert!(take_surface_actions(app.world_mut()).iter().all(|action| {
        !matches!(
            action,
            SurfaceAction::Resize { surface, .. }
                | SurfaceAction::SetOutputs { surface, .. }
                if *surface == source_surface || *surface == destination
        )
    }));

    app.world_mut()
        .get_mut::<WindowGeometry>(receiver)
        .expect("receiver geometry")
        .size += Vec2::new(64.0, 32.0);
    app.update();
    assert!(take_surface_actions(app.world_mut()).iter().any(|action| {
        matches!(action, SurfaceAction::Resize { surface, .. } if *surface == destination)
    }));

    app.world_mut()
        .get_mut::<WindowGeometry>(source_window)
        .expect("placeholder geometry")
        .size += Vec2::new(48.0, 24.0);
    app.update();
    assert!(take_surface_actions(app.world_mut()).iter().all(|action| {
        !matches!(action, SurfaceAction::Resize { surface, .. } if *surface == source_surface || *surface == destination)
    }));
}

#[test]
fn relayed_csd_interaction_targets_the_receiver_not_the_placeholder() {
    let (mut app, endpoint) = test_app();
    let source_surface = surface(LOCAL_SOURCE, 34);
    let source_client = map_surface(&mut app, source_surface, None, WindowDecoration::ClientSide);
    let source_window = window_for_client(&mut app, source_client);
    let source_geometry = *app
        .world()
        .get::<WindowGeometry>(source_window)
        .expect("source geometry");
    let (_, receiver) = begin_hoist(
        &mut app,
        endpoint,
        source_window,
        source_surface,
        WindowDecoration::ClientSide,
    );
    let destination = endpoint.destination(source_surface);

    enqueue_surface_event(
        app.world_mut(),
        HostSurfaceEvent {
            surface: destination,
            kind: HostSurfaceEventKind::Interaction(ToplevelInteractionRequestKind::Move),
        },
    );
    app.update();

    assert_eq!(
        app.world()
            .get::<WindowInteractionSession>(receiver)
            .map(|interaction| interaction.kind),
        Some(WindowInteractionKind::Move)
    );
    assert!(
        app.world()
            .get::<WindowInteractionSession>(source_window)
            .is_none()
    );
    assert_eq!(
        app.world().get::<WindowGeometry>(source_window),
        Some(&source_geometry)
    );
}

#[test]
fn reparented_member_restores_without_ending_the_original_family() {
    let (mut app, endpoint) = test_app();
    let root_surface = surface(LOCAL_SOURCE, 40);
    let child_surface = surface(LOCAL_SOURCE, 41);
    let root_client = map_surface(&mut app, root_surface, None, WindowDecoration::ServerSide);
    let child_client = map_surface(&mut app, child_surface, None, WindowDecoration::ServerSide);
    let root_window = window_for_client(&mut app, root_client);
    let child_window = window_for_client(&mut app, child_client);
    app.world_mut().write_message(HoistWindow {
        window: root_window,
    });
    app.update();
    let root_destination = endpoint.destination(root_surface);
    let child_destination = endpoint.destination(child_surface);
    let _root_destination_client = map_surface(
        &mut app,
        root_destination,
        None,
        WindowDecoration::ServerSide,
    );
    let _child_destination_client = map_surface(
        &mut app,
        child_destination,
        None,
        WindowDecoration::ServerSide,
    );
    app.update();

    let child_session = app
        .world_mut()
        .query::<(bevy::ecs::entity::Entity, &HoistSession)>()
        .iter(app.world())
        .find_map(|(entity, session)| (session.surface() == child_surface).then_some(entity))
        .expect("child session");
    assert!(matches!(
        app.world()
            .get::<HoistSession>(child_session)
            .map(|session| session.membership),
        Some(crate::HoistMembership::ClientPeer { .. })
    ));

    set_parent(&mut app, child_surface, Some(root_surface));
    app.update();
    assert!(matches!(
        app.world()
            .get::<HoistSession>(child_session)
            .map(|session| session.membership),
        Some(crate::HoistMembership::DeclaredFamily { group_root }) if group_root == root_surface
    ));

    set_parent(&mut app, child_surface, None);
    app.update();
    assert!(matches!(
        app.world()
            .get::<HoistSession>(child_session)
            .map(|session| session.state),
        Some(SessionState::Unmapping)
    ));

    destroy_surface(&mut app, child_destination);
    app.update();
    app.update();
    assert_eq!(
        app.world()
            .get::<OccupiesWindow>(child_client)
            .map(|occupancy| occupancy.0),
        Some(child_window)
    );
    assert!(app.world().get::<HoistPlaceholder>(root_window).is_some());
    assert!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .any(|session| session.surface() == root_surface)
    );

    app.update();
    assert_eq!(
        app.world()
            .get::<OccupiesWindow>(child_client)
            .map(|occupancy| occupancy.0),
        Some(child_window)
    );
    assert!(
        !app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .any(|session| session.surface() == child_surface)
    );

    app.world_mut().write_message(HoistWindow {
        window: child_window,
    });
    app.update();
    assert!(app.world().get::<OccupiesWindow>(child_client).is_none());
    assert!(app.world().entities().contains(child_window));
    assert!(app.world().get::<HoistPlaceholder>(child_window).is_some());
    let reopted = app
        .world_mut()
        .query::<&HoistSession>()
        .iter(app.world())
        .find(|session| session.surface() == child_surface)
        .expect("re-opted session");
    assert_eq!(reopted.source_mode(), crate::HoistSourceMode::PreservedSlot);
}

#[test]
fn root_unmap_keeps_the_surviving_family_placeholder() {
    let (mut app, endpoint) = test_app();
    let root_surface = surface(LOCAL_SOURCE, 42);
    let child_surface = surface(LOCAL_SOURCE, 43);
    let root_client = map_surface(&mut app, root_surface, None, WindowDecoration::ServerSide);
    let child_client = map_surface(
        &mut app,
        child_surface,
        Some(root_surface),
        WindowDecoration::ServerSide,
    );
    let root_window = window_for_client(&mut app, root_client);
    let child_window = window_for_client(&mut app, child_client);
    app.world_mut().write_message(HoistWindow {
        window: root_window,
    });
    app.update();
    let root_destination = endpoint.destination(root_surface);
    let child_destination = endpoint.destination(child_surface);
    let _root_destination_client = map_surface(
        &mut app,
        root_destination,
        None,
        WindowDecoration::ServerSide,
    );
    let _child_destination_client = map_surface(
        &mut app,
        child_destination,
        Some(root_destination),
        WindowDecoration::ServerSide,
    );
    app.update();

    unmap_surface(&mut app, root_surface);
    app.update();
    destroy_surface(&mut app, root_destination);
    app.update();
    app.update();

    assert!(app.world().get::<HoistPlaceholder>(child_window).is_some());
    assert!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .any(|session| session.surface() == child_surface)
    );
    assert!(app.world().entities().contains(child_window));

    commit_surface(&mut app, root_surface, UVec2::new(320, 240));
    app.update();
    app.update();
    assert!(app.world().get::<OccupiesWindow>(root_client).is_some());
    assert!(
        app.world()
            .get::<WindowAdmissionHold>(root_client)
            .is_none()
    );
    assert!(
        !app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .any(|session| session.surface() == root_surface)
    );
}

#[test]
fn active_family_keeps_its_endpoint_after_the_default_changes() {
    let (mut app, first_loopback) = test_app();
    let first = RecordingEndpoint::new(first_loopback);
    let (_, second_loopback) = loopback_registration(LOCAL_SOURCE, ClientSourceId::new(2));
    let second = RecordingEndpoint::new(second_loopback);
    let mut endpoints = HoistEndpointRegistry::with_default(first.clone());
    let first_id = endpoints.default_id().expect("first endpoint");
    let second_id = endpoints.register(second.clone()).expect("second endpoint");
    let namespace_probe = surface(LOCAL_SOURCE, 999);
    assert_ne!(
        first.destination(namespace_probe),
        second.destination(namespace_probe)
    );
    app.world_mut().insert_resource(endpoints);

    let root_surface = surface(LOCAL_SOURCE, 70);
    let root_client = map_surface(&mut app, root_surface, None, WindowDecoration::ServerSide);
    let root_window = window_for_client(&mut app, root_client);
    app.world_mut().write_message(HoistWindow {
        window: root_window,
    });
    app.update();
    assert!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .all(|session| session.endpoint() == first_id)
    );

    assert!(
        app.world_mut()
            .resource_mut::<HoistEndpointRegistry>()
            .set_default(second_id)
    );
    first.take_calls();
    second.take_calls();
    let child_surface = surface(LOCAL_SOURCE, 71);
    let _child_client = map_surface(
        &mut app,
        child_surface,
        Some(root_surface),
        WindowDecoration::ServerSide,
    );
    app.update();

    let session_endpoints = app
        .world_mut()
        .query::<&HoistSession>()
        .iter(app.world())
        .map(HoistSession::endpoint)
        .collect::<Vec<_>>();
    assert_eq!(session_endpoints.len(), 2);
    assert!(
        session_endpoints
            .iter()
            .all(|endpoint| *endpoint == first_id)
    );

    first.take_calls();
    second.take_calls();
    unmap_surface(&mut app, root_surface);
    app.update();
    assert_eq!(first.take_calls(), vec![EndpointCall::Unmap(root_surface)]);
    assert!(second.take_calls().is_empty());
}

#[test]
fn unavailable_default_refuses_admission_without_mutating_the_source() {
    let (mut app, loopback) = test_app();
    let endpoint = RecordingEndpoint::new(loopback);
    endpoint.set_available(false);
    app.world_mut()
        .insert_resource(HoistEndpointRegistry::with_default(endpoint.clone()));
    let source_surface = surface(LOCAL_SOURCE, 72);
    let source_client = map_surface(&mut app, source_surface, None, WindowDecoration::ServerSide);
    let source_window = window_for_client(&mut app, source_client);
    let detached_family = HoistFamilyId::new(44);
    app.world_mut()
        .entity_mut(source_client)
        .insert(HoistDetached {
            family: detached_family,
        });

    app.world_mut().write_message(HoistWindow {
        window: source_window,
    });
    app.update();

    assert_eq!(
        app.world()
            .get::<HoistDetached>(source_client)
            .map(|detached| detached.family),
        Some(detached_family)
    );
    assert_eq!(
        app.world()
            .get::<OccupiesWindow>(source_client)
            .map(|occupancy| occupancy.0),
        Some(source_window)
    );
    assert_eq!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .count(),
        0
    );
    assert!(endpoint.take_calls().is_empty());
}

#[test]
fn unavailable_session_endpoint_restores_its_source() {
    let (mut app, loopback) = test_app();
    let endpoint = RecordingEndpoint::new(loopback);
    app.world_mut()
        .insert_resource(HoistEndpointRegistry::with_default(endpoint.clone()));
    let source_surface = surface(LOCAL_SOURCE, 73);
    let source_client = map_surface(&mut app, source_surface, None, WindowDecoration::ServerSide);
    let source_window = window_for_client(&mut app, source_client);
    app.world_mut().write_message(HoistWindow {
        window: source_window,
    });
    app.update();
    assert!(app.world().get::<OccupiesWindow>(source_client).is_none());
    assert!(app.world().get::<HoistPlaceholder>(source_window).is_some());

    endpoint.set_available(false);
    app.update();

    assert_eq!(
        app.world()
            .get::<OccupiesWindow>(source_client)
            .map(|occupancy| occupancy.0),
        Some(source_window)
    );
    assert!(app.world().get::<HoistPlaceholder>(source_window).is_none());
    assert_eq!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .count(),
        0
    );
}

#[test]
fn destroyed_preserved_source_becomes_a_dismissible_tombstone() {
    let (mut app, endpoint) = test_app();
    let source_surface = surface(LOCAL_SOURCE, 50);
    let source_client = map_surface(&mut app, source_surface, None, WindowDecoration::ServerSide);
    let source_window = window_for_client(&mut app, source_client);
    let (session, _) = begin_hoist(
        &mut app,
        endpoint,
        source_window,
        source_surface,
        WindowDecoration::ServerSide,
    );

    destroy_surface(&mut app, source_surface);
    app.update();
    assert!(matches!(
        app.world()
            .get::<HoistSession>(session)
            .map(|session| session.state),
        Some(SessionState::Closed)
    ));
    assert_eq!(
        app.world()
            .get::<HoistPlaceholder>(source_window)
            .map(|placeholder| placeholder.state),
        Some(crate::HoistPlaceholderState::Closed)
    );

    destroy_surface(&mut app, endpoint.destination(source_surface));
    app.update();
    app.world_mut()
        .resource_mut::<Messages<RequestRedraw>>()
        .clear();
    app.world_mut()
        .write_message(DismissHoistTombstone { session });
    app.update();
    assert!(!app.world().resource::<Messages<RequestRedraw>>().is_empty());
    assert!(!app.world().entities().contains(session));
    assert!(!app.world().entities().contains(source_window));
}
