use std::time::Instant;

use bevy::{
    app::App,
    asset::{AssetApp, AssetPlugin, Assets},
    color::Color,
    ecs::{entity::Entity, hierarchy::Children},
    image::Image,
    math::{UVec2, Vec2},
    scene::ScenePlugin,
    ui::{BorderColor, UiScale},
    window::RequestRedraw,
};
use weld_app::{
    input::GlobalShortcutPressed,
    output::{OutputGeometry, OutputId, OutputPosition, PrimaryOutput, WeldOutput},
    surface::{
        ClientPopup, ClientToplevel, HostSurfaceEvent, HostSurfaceEventKind, SurfaceAction,
        SurfaceBufferContent, SurfaceBufferUpdate, SurfaceContentView, SurfaceId, SurfaceLayerId,
        SurfaceLayerPlacement, SurfacePlugin, SurfaceTreeSnapshot, SurfaceWindowGeometry,
        ToplevelInteractionRequest, ToplevelInteractionRequestKind, WindowDecoration,
        enqueue_surface_event, take_surface_actions,
    },
};
use weld_float::FloatPlugin;
use weld_ssd::SsdPlugin;
use weld_window::{
    FocusedWindow, OccupiesWindow, PresentationInsets, PresentationOffset,
    PrimaryWindowPresentation, WindowClientBinding, WindowGeometry, WindowGeometryAnchor,
    WindowInteractionKind, WindowInteractionSession, WindowOccupant, WindowOutput, WindowPlugin,
    WindowPresentationOverride, WindowProjection, WindowVisibility,
};
use weld_window_ui::{PrimarySurfacePresentation, WindowUiPlugin};

use crate::{
    DismissHoistTombstone, HoistPlugin, HoistSession, HoistSessionPhase, HoistShortcut,
    HoistSourceMode, HoistWindow, HoistedWindow, ReclaimHoist,
    presentation::{
        DismissHoistTombstoneHandle, HoistPlaceholderState, HoistPresentation, ReclaimHoistHandle,
    },
};

fn test_app() -> App {
    let mut app = App::new();
    app.add_plugins((
        bevy::app::TaskPoolPlugin::default(),
        AssetPlugin::default(),
        ScenePlugin,
    ));
    app.init_asset::<bevy::shader::Shader>()
        .insert_resource(Assets::<Image>::default())
        .insert_resource(UiScale(1.0))
        .add_message::<RequestRedraw>()
        .add_plugins((
            SurfacePlugin,
            WindowPlugin,
            WindowUiPlugin,
            SsdPlugin,
            FloatPlugin,
            HoistPlugin,
        ));
    app.world_mut().spawn((
        WeldOutput {
            id: OutputId::new(1),
        },
        OutputGeometry::from_physical(UVec2::new(1_000, 800), 1.0),
        OutputPosition::default(),
        PrimaryOutput,
    ));
    app
}

fn map_server_decorated_surface(app: &mut App, surface: SurfaceId) -> Entity {
    map_surface(
        app,
        surface,
        WindowDecoration::ServerSide,
        UVec2::new(320, 240),
        Vec2::ZERO,
        UVec2::new(320, 240),
    )
}

fn set_toplevel_parent(app: &mut App, child: SurfaceId, parent: Option<SurfaceId>) {
    enqueue_surface_event(
        app.world_mut(),
        HostSurfaceEvent {
            surface: child,
            kind: HostSurfaceEventKind::ToplevelParentChanged { parent },
        },
    );
    app.update();
}

fn commit_server_decorated_surface(app: &mut App, surface: SurfaceId, size: UVec2) {
    enqueue_surface_event(
        app.world_mut(),
        frame_event(surface, size, Vec2::ZERO, size),
    );
}

fn map_surface(
    app: &mut App,
    surface: SurfaceId,
    decoration: WindowDecoration,
    root_size: UVec2,
    geometry_origin: Vec2,
    geometry_size: UVec2,
) -> Entity {
    enqueue_surface_event(
        app.world_mut(),
        HostSurfaceEvent {
            surface,
            kind: HostSurfaceEventKind::Created { decoration },
        },
    );
    enqueue_surface_event(
        app.world_mut(),
        frame_event(surface, root_size, geometry_origin, geometry_size),
    );
    app.update();
    app.world_mut()
        .query::<(&ClientToplevel, &OccupiesWindow)>()
        .iter(app.world())
        .find_map(|(toplevel, occupancy)| (toplevel.surface == surface).then_some(occupancy.0))
        .expect("mapped source should be admitted")
}

fn frame_event(
    surface: SurfaceId,
    root_size: UVec2,
    geometry_origin: Vec2,
    geometry_size: UVec2,
) -> HostSurfaceEvent {
    let root_view = SurfaceContentView {
        source_x: 0.0,
        source_y: 0.0,
        source_width: root_size.x as f32,
        source_height: root_size.y as f32,
        logical_width: root_size.x as f32,
        logical_height: root_size.y as f32,
    };
    let geometry_view = SurfaceContentView {
        source_x: geometry_origin.x,
        source_y: geometry_origin.y,
        source_width: geometry_size.x as f32,
        source_height: geometry_size.y as f32,
        logical_width: geometry_size.x as f32,
        logical_height: geometry_size.y as f32,
    };
    HostSurfaceEvent {
        surface,
        kind: HostSurfaceEventKind::TreeSnapshot(SurfaceTreeSnapshot {
            client_mapped: true,
            root: Some(SurfaceLayerPlacement {
                layer: SurfaceLayerId::new(1),
                position: Vec2::ZERO,
                view: root_view,
            }),
            window_geometry: Some(SurfaceWindowGeometry {
                origin: geometry_origin,
                view: geometry_view,
            }),
            overlays: Vec::new(),
            inputs: Vec::new(),
            buffers: vec![SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(1),
                width: root_size.x,
                height: root_size.y,
                content: SurfaceBufferContent::Pixels(vec![
                    0;
                    root_size.x as usize
                        * root_size.y as usize
                        * 4
                ]),
                opaque: true,
            }],
        }),
    }
}

fn projection_count(app: &mut App, window: Entity) -> usize {
    app.world_mut()
        .query::<&WindowProjection>()
        .iter(app.world())
        .filter(|projection| projection.window() == window)
        .count()
}

#[test]
fn reclaim_waits_for_the_placeholder_sized_client_commit() {
    let mut app = test_app();
    let surface = SurfaceId::new(41);
    let source = map_server_decorated_surface(&mut app, surface);
    let occupant = app
        .world()
        .get::<WindowOccupant>(source)
        .expect("source should retain an occupant")
        .entity();
    let original_geometry = *app
        .world()
        .get::<WindowGeometry>(source)
        .expect("source should have managed geometry");

    app.world_mut()
        .write_message(HoistWindow { window: source });
    app.update();

    let (session_entity, session) = app
        .world_mut()
        .query::<(Entity, &HoistSession)>()
        .single(app.world())
        .map(|(entity, session)| (entity, *session))
        .expect("hoist should create one session");
    let receiver = session.receiver();
    assert_eq!(session.source(), source);
    assert_eq!(session.surface(), surface);
    assert_eq!(
        app.world()
            .get::<WindowOccupant>(source)
            .expect("hoisting must not detach the source occupant")
            .entity(),
        occupant
    );
    assert_eq!(
        app.world().get::<WindowGeometry>(source),
        Some(&original_geometry)
    );
    assert_eq!(projection_count(&mut app, source), 1);
    assert_eq!(projection_count(&mut app, receiver), 1);
    let source_root = app
        .world()
        .get::<PrimaryWindowPresentation>(source)
        .expect("source should have a hoist placeholder")
        .entity();
    let receiver_root = app
        .world()
        .get::<PrimaryWindowPresentation>(receiver)
        .expect("receiver should use an ordinary presentation")
        .entity();
    assert!(app.world().get::<HoistPresentation>(source_root).is_some());
    assert!(
        app.world()
            .get::<HoistPresentation>(receiver_root)
            .is_none()
    );
    assert_eq!(
        app.world().get::<PresentationInsets>(receiver_root),
        Some(&PresentationInsets::new(3.0, 33.0, 3.0, 3.0))
    );
    assert_eq!(
        app.world().resource::<FocusedWindow>().entity(),
        Some(receiver)
    );
    assert!(
        take_surface_actions(app.world_mut())
            .into_iter()
            .any(|action| matches!(action, SurfaceAction::Focus { surface: Some(focused) } if focused == surface))
    );

    app.world_mut()
        .get_mut::<WindowGeometry>(receiver)
        .expect("receiver should have geometry")
        .size += Vec2::new(96.0, 64.0);
    app.update();
    enqueue_surface_event(
        app.world_mut(),
        frame_event(
            surface,
            UVec2::new(416, 304),
            Vec2::ZERO,
            UVec2::new(416, 304),
        ),
    );
    app.update();
    take_surface_actions(app.world_mut());

    app.world_mut().write_message(ReclaimHoist {
        session: session_entity,
    });
    app.update();

    assert!(app.world().entities().contains(receiver));
    assert!(app.world().entities().contains(session_entity));
    assert_eq!(
        app.world().get::<WindowVisibility>(receiver),
        Some(&WindowVisibility::Hidden)
    );
    assert!(app.world().get::<HoistedWindow>(source).is_some());
    assert!(
        take_surface_actions(app.world_mut())
            .into_iter()
            .any(|action| action
                == SurfaceAction::Resize {
                    surface,
                    logical_size: UVec2::new(320, 240),
                })
    );

    app.world_mut().write_message(ReclaimHoist {
        session: session_entity,
    });
    app.update();
    assert!(app.world().entities().contains(session_entity));

    enqueue_surface_event(
        app.world_mut(),
        frame_event(
            surface,
            UVec2::new(320, 240),
            Vec2::ZERO,
            UVec2::new(320, 240),
        ),
    );
    app.update();

    assert!(!app.world().entities().contains(receiver));
    assert!(!app.world().entities().contains(session_entity));
    assert!(app.world().get::<HoistedWindow>(source).is_none());
    assert!(
        app.world()
            .get::<WindowPresentationOverride>(source)
            .is_none()
    );
    assert!(app.world().get::<WindowClientBinding>(source).is_none());
    assert_eq!(
        app.world()
            .get::<WindowOccupant>(source)
            .expect("reclaim should retain the original occupant")
            .entity(),
        occupant
    );
    assert_eq!(
        app.world().get::<WindowGeometry>(source),
        Some(&original_geometry)
    );
    app.update();
    assert_eq!(projection_count(&mut app, source), 1);
    let restored_root = app
        .world()
        .get::<PrimaryWindowPresentation>(source)
        .expect("ordinary source presentation should return")
        .entity();
    assert!(
        app.world()
            .get::<HoistPresentation>(restored_root)
            .is_none()
    );
}

#[test]
fn csd_shortcut_keeps_placeholder_at_geometry_origin_and_receiver_at_visual_origin() {
    let mut app = test_app();
    let surface = SurfaceId::new(61);
    let visual_offset = Vec2::new(-20.0, -18.0);
    let source = map_surface(
        &mut app,
        surface,
        WindowDecoration::ClientSide,
        UVec2::new(360, 276),
        -visual_offset,
        UVec2::new(320, 240),
    );
    let original_geometry = *app
        .world()
        .get::<WindowGeometry>(source)
        .expect("CSD source should have managed geometry");
    let popup = SurfaceId::new(62);
    enqueue_surface_event(
        app.world_mut(),
        HostSurfaceEvent {
            surface: popup,
            kind: HostSurfaceEventKind::PopupConfigured(ClientPopup {
                owner: surface,
                position: Vec2::new(80.0, 40.0),
                stack_index: 1,
            }),
        },
    );
    enqueue_surface_event(
        app.world_mut(),
        frame_event(popup, UVec2::new(120, 80), Vec2::ZERO, UVec2::new(120, 80)),
    );
    app.update();
    assert!(
        app.world_mut()
            .query::<(&ClientPopup, Option<&PrimarySurfacePresentation>)>()
            .single(app.world())
            .expect("mapped popup should exist")
            .1
            .is_some()
    );

    let shortcut = app.world().resource::<HoistShortcut>().0;
    app.world_mut()
        .write_message(GlobalShortcutPressed::new(shortcut));
    app.update();

    let session = *app
        .world_mut()
        .query::<&HoistSession>()
        .single(app.world())
        .expect("shortcut should create a hoist session");
    let placeholder = app
        .world()
        .get::<PrimaryWindowPresentation>(source)
        .expect("source should have a placeholder")
        .entity();
    let receiver = app
        .world()
        .get::<PrimaryWindowPresentation>(session.receiver())
        .expect("receiver should have a loopback presentation")
        .entity();
    assert_eq!(
        app.world().get::<PresentationOffset>(placeholder),
        Some(&PresentationOffset::default())
    );
    assert_eq!(
        app.world().get::<WindowGeometryAnchor>(placeholder),
        Some(&WindowGeometryAnchor::default())
    );
    assert_eq!(
        app.world().get::<PresentationOffset>(receiver),
        Some(&PresentationOffset(visual_offset))
    );
    assert_eq!(
        app.world().get::<WindowGeometryAnchor>(receiver),
        Some(&WindowGeometryAnchor(-visual_offset))
    );
    assert_eq!(
        app.world().get::<WindowGeometry>(source),
        Some(&original_geometry)
    );
    let popup_root = app
        .world_mut()
        .query::<(&ClientPopup, &PrimarySurfacePresentation)>()
        .single(app.world())
        .expect("popup should follow the receiver")
        .1
        .entity();
    assert_eq!(
        app.world()
            .get::<bevy::ecs::hierarchy::ChildOf>(popup_root)
            .map(bevy::ecs::hierarchy::ChildOf::parent),
        Some(receiver)
    );
    app.world_mut().write_message(ToplevelInteractionRequest {
        surface,
        kind: ToplevelInteractionRequestKind::Move,
    });
    app.update();
    assert_eq!(
        app.world()
            .get::<WindowInteractionSession>(session.receiver())
            .map(|interaction| interaction.kind),
        Some(WindowInteractionKind::Move)
    );
    assert!(
        app.world()
            .get::<WindowInteractionSession>(source)
            .is_none()
    );
}

#[test]
fn removing_the_loopback_receiver_recovers_the_source() {
    let mut app = test_app();
    let source = map_server_decorated_surface(&mut app, SurfaceId::new(52));
    app.world_mut()
        .write_message(HoistWindow { window: source });
    app.update();
    let receiver = app
        .world_mut()
        .query::<&HoistSession>()
        .single(app.world())
        .expect("hoist should create a session")
        .receiver();

    app.world_mut().despawn(receiver);
    app.update();

    assert!(
        app.world()
            .get::<WindowPresentationOverride>(source)
            .is_none()
    );
    assert!(app.world().get::<HoistedWindow>(source).is_none());
    assert!(
        app.world()
            .get::<PrimaryWindowPresentation>(source)
            .is_some()
    );
}

#[test]
fn receiver_owns_output_membership_and_client_resize_until_reclaim() {
    let mut app = test_app();
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
    let surface = SurfaceId::new(68);
    let source = map_server_decorated_surface(&mut app, surface);
    app.world_mut()
        .write_message(HoistWindow { window: source });
    app.update();
    let (session, receiver) = app
        .world_mut()
        .query::<(Entity, &HoistSession)>()
        .single(app.world())
        .map(|(entity, session)| (entity, session.receiver()))
        .expect("hoist should create a session");
    take_surface_actions(app.world_mut());

    app.world_mut()
        .entity_mut(receiver)
        .insert(WindowOutput(secondary));
    app.world_mut()
        .get_mut::<WindowGeometry>(receiver)
        .expect("receiver should have geometry")
        .position = Vec2::new(100.0, 100.0);
    app.update();
    let receiver_output_actions = take_surface_actions(app.world_mut());
    assert!(receiver_output_actions.iter().any(|action| {
        matches!(
            action,
            SurfaceAction::SetOutputs {
                surface: target,
                outputs,
                preferred: Some(preferred),
            } if *target == surface && outputs == &[OutputId::new(2)] && *preferred == OutputId::new(2)
        )
    }));

    app.world_mut()
        .get_mut::<WindowGeometry>(source)
        .expect("source should have geometry")
        .position = Vec2::new(160.0, 120.0);
    app.update();
    assert!(
        take_surface_actions(app.world_mut())
            .into_iter()
            .all(|action| !matches!(action, SurfaceAction::SetOutputs { surface: target, .. } if target == surface))
    );

    app.world_mut()
        .get_mut::<WindowGeometry>(receiver)
        .expect("receiver should have geometry")
        .size += Vec2::new(96.0, 64.0);
    app.update();
    assert!(
        take_surface_actions(app.world_mut())
            .into_iter()
            .any(|action| matches!(action, SurfaceAction::Resize { surface: target, .. } if target == surface))
    );

    app.world_mut()
        .get_mut::<WindowGeometry>(source)
        .expect("source should have geometry")
        .size += Vec2::new(48.0, 32.0);
    app.update();
    assert!(
        take_surface_actions(app.world_mut())
            .into_iter()
            .all(|action| !matches!(action, SurfaceAction::Resize { surface: target, .. } if target == surface))
    );

    app.world_mut().write_message(ReclaimHoist { session });
    app.update();
    let reclaim_actions = take_surface_actions(app.world_mut());
    assert!(reclaim_actions.iter().any(|action| {
        matches!(action, SurfaceAction::Resize { surface: target, .. } if *target == surface)
    }));
    assert!(reclaim_actions.iter().any(|action| {
        matches!(
            action,
            SurfaceAction::SetOutputs {
                surface: target,
                outputs,
                preferred: Some(preferred),
            } if *target == surface && outputs == &[OutputId::new(1)] && *preferred == OutputId::new(1)
        )
    }));

    enqueue_surface_event(
        app.world_mut(),
        frame_event(
            surface,
            UVec2::new(320, 240),
            Vec2::ZERO,
            UVec2::new(320, 240),
        ),
    );
    app.update();
    assert!(!app.world().entities().contains(session));
    assert!(!app.world().entities().contains(receiver));
}

#[test]
fn reclaim_timeout_restores_the_source_when_the_client_does_not_commit() {
    let mut app = test_app();
    let surface = SurfaceId::new(69);
    let source = map_server_decorated_surface(&mut app, surface);
    app.world_mut()
        .write_message(HoistWindow { window: source });
    app.update();
    let (session, receiver) = app
        .world_mut()
        .query::<(Entity, &HoistSession)>()
        .single(app.world())
        .map(|(entity, session)| (entity, session.receiver()))
        .expect("hoist should create a session");

    app.world_mut()
        .get_mut::<WindowGeometry>(receiver)
        .expect("receiver should have geometry")
        .size += Vec2::new(80.0, 40.0);
    app.update();
    app.world_mut().write_message(ReclaimHoist { session });
    app.update();
    let mut session_state = app
        .world_mut()
        .get_mut::<HoistSession>(session)
        .expect("reclaim should wait for the client configure");
    let HoistSessionPhase::Reclaiming {
        scope,
        target_size,
        resize_required,
        resize_request_observed,
        ..
    } = session_state.phase
    else {
        panic!("reclaim should enter the configuring phase");
    };
    session_state.phase = HoistSessionPhase::Reclaiming {
        scope,
        target_size,
        resize_required,
        resize_request_observed,
        deadline: Instant::now(),
    };

    app.update();

    assert!(!app.world().entities().contains(session));
    assert!(!app.world().entities().contains(receiver));
    assert!(app.world().get::<WindowClientBinding>(source).is_none());
}

#[test]
fn hoist_follows_existing_and_later_toplevel_family_members() {
    let mut app = test_app();
    let root_surface = SurfaceId::new(90);
    let child_surface = SurfaceId::new(91);
    let later_surface = SurfaceId::new(92);
    let mid_reclaim_surface = SurfaceId::new(93);
    let root = map_server_decorated_surface(&mut app, root_surface);
    let child = map_server_decorated_surface(&mut app, child_surface);
    set_toplevel_parent(&mut app, child_surface, Some(root_surface));
    let root_source_geometry = *app
        .world()
        .get::<WindowGeometry>(root)
        .expect("root should retain source geometry");
    let child_source_geometry = *app
        .world()
        .get::<WindowGeometry>(child)
        .expect("child should retain source geometry");

    app.world_mut().write_message(HoistWindow { window: root });
    app.update();
    let sessions = app
        .world_mut()
        .query::<(Entity, &HoistSession)>()
        .iter(app.world())
        .map(|(entity, session)| (entity, *session))
        .collect::<Vec<_>>();
    assert_eq!(sessions.len(), 2);
    assert!(sessions.iter().all(|(_, session)| {
        session.family() == sessions[0].1.family() && session.family_root() == root_surface
    }));
    assert!(
        sessions
            .iter()
            .all(|(_, session)| session.source_mode == HoistSourceMode::PreservedSlot)
    );
    assert_eq!(projection_count(&mut app, root), 1);
    assert_eq!(projection_count(&mut app, child), 1);
    assert_eq!(
        app.world_mut()
            .query::<&HoistPresentation>()
            .iter(app.world())
            .count(),
        2
    );
    assert_eq!(
        app.world().get::<WindowGeometry>(root),
        Some(&root_source_geometry)
    );
    assert_eq!(
        app.world().get::<WindowGeometry>(child),
        Some(&child_source_geometry)
    );
    for (_, session) in &sessions {
        let receiver_root = app
            .world()
            .get::<PrimaryWindowPresentation>(session.receiver())
            .expect("family receiver should have ordinary SSD")
            .entity();
        let border = app
            .world()
            .get::<BorderColor>(receiver_root)
            .expect("SSD receiver should expose its border color");
        assert!(
            *border == BorderColor::all(Color::srgb(0.92, 0.18, 0.16))
                || *border == BorderColor::all(Color::srgb(0.58, 0.12, 0.12))
        );
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(session.receiver())
                .map(|geometry| geometry.size),
            Some(Vec2::new(326.0, 276.0))
        );
    }

    let later = map_server_decorated_surface(&mut app, later_surface);
    let later_source_geometry = *app
        .world()
        .get::<WindowGeometry>(later)
        .expect("later child should retain source geometry");
    set_toplevel_parent(&mut app, later_surface, Some(root_surface));
    app.update();
    assert!(app.world().get::<HoistedWindow>(later).is_some());
    let later_session = app
        .world_mut()
        .query::<&HoistSession>()
        .iter(app.world())
        .find(|session| session.source() == later)
        .copied()
        .expect("later child should have a family session");
    assert_eq!(later_session.source_mode, HoistSourceMode::Followed);
    assert_eq!(projection_count(&mut app, later), 0);
    assert_eq!(
        app.world().get::<WindowGeometry>(later),
        Some(&later_source_geometry)
    );
    assert_eq!(
        app.world_mut()
            .query::<&HoistPresentation>()
            .iter(app.world())
            .count(),
        2
    );
    app.world_mut().write_message(HoistWindow { window: later });
    app.update();
    assert_eq!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .find(|session| session.source() == later)
            .map(|session| session.source_mode),
        Some(HoistSourceMode::Followed)
    );
    assert_eq!(projection_count(&mut app, later), 0);
    let sessions = app
        .world_mut()
        .query::<(Entity, &HoistSession)>()
        .iter(app.world())
        .map(|(entity, session)| (entity, *session))
        .collect::<Vec<_>>();
    assert_eq!(sessions.len(), 3);

    app.world_mut().write_message(ReclaimHoist {
        session: sessions[0].0,
    });
    app.update();
    assert!(sessions.iter().all(|(_, session)| {
        app.world().get::<WindowVisibility>(session.receiver()) == Some(&WindowVisibility::Hidden)
    }));

    let mid_reclaim = map_server_decorated_surface(&mut app, mid_reclaim_surface);
    set_toplevel_parent(&mut app, mid_reclaim_surface, Some(root_surface));
    app.update();
    assert!(app.world().get::<HoistedWindow>(mid_reclaim).is_none());

    commit_server_decorated_surface(&mut app, root_surface, UVec2::new(320, 240));
    commit_server_decorated_surface(&mut app, child_surface, UVec2::new(320, 240));
    app.update();
    assert_eq!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .count(),
        3
    );

    commit_server_decorated_surface(&mut app, later_surface, UVec2::new(320, 240));
    app.update();
    assert_eq!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .count(),
        0
    );
    assert!(app.world().get::<HoistedWindow>(root).is_none());
    assert!(app.world().get::<HoistedWindow>(child).is_none());
    assert!(app.world().get::<HoistedWindow>(later).is_none());
}

#[test]
fn removing_a_parent_stages_only_that_family_member_for_restore() {
    let mut app = test_app();
    let root_surface = SurfaceId::new(94);
    let child_surface = SurfaceId::new(95);
    let root = map_server_decorated_surface(&mut app, root_surface);
    app.world_mut().write_message(HoistWindow { window: root });
    app.update();
    commit_server_decorated_surface(&mut app, root_surface, UVec2::new(320, 240));
    app.update();

    let child = map_server_decorated_surface(&mut app, child_surface);
    let child_source_geometry = *app
        .world()
        .get::<WindowGeometry>(child)
        .expect("child should retain source geometry");
    set_toplevel_parent(&mut app, child_surface, Some(root_surface));
    app.update();
    assert_eq!(projection_count(&mut app, child), 0);
    commit_server_decorated_surface(&mut app, child_surface, UVec2::new(320, 240));
    app.update();
    let child_receiver = app
        .world_mut()
        .query::<&HoistSession>()
        .iter(app.world())
        .find(|session| session.source() == child)
        .map(HoistSession::receiver)
        .expect("child should have a receiver");
    app.world_mut()
        .get_mut::<WindowGeometry>(child_receiver)
        .expect("child receiver should have geometry")
        .size += Vec2::new(80.0, 40.0);
    app.update();
    commit_server_decorated_surface(&mut app, child_surface, UVec2::new(400, 280));
    app.update();
    take_surface_actions(app.world_mut());

    set_toplevel_parent(&mut app, child_surface, None);
    assert!(
        take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
            surface: child_surface,
            logical_size: UVec2::new(320, 240),
        })
    );
    let child_session = app
        .world_mut()
        .query::<&HoistSession>()
        .iter(app.world())
        .find(|session| session.source() == child)
        .copied()
        .expect("child should retain its session while staged");
    assert_eq!(
        app.world()
            .get::<WindowVisibility>(child_session.receiver()),
        Some(&WindowVisibility::Hidden)
    );
    let root_session = app
        .world_mut()
        .query::<&HoistSession>()
        .iter(app.world())
        .find(|session| session.source() == root)
        .copied()
        .expect("root should remain hoisted");
    assert_eq!(
        app.world().get::<WindowVisibility>(root_session.receiver()),
        Some(&WindowVisibility::Visible)
    );

    commit_server_decorated_surface(&mut app, child_surface, UVec2::new(320, 240));
    app.update();
    assert!(app.world().get::<HoistedWindow>(child).is_none());
    assert!(app.world().get::<HoistedWindow>(root).is_some());
    assert_eq!(
        app.world().get::<WindowGeometry>(child),
        Some(&child_source_geometry)
    );
    assert_eq!(
        app.world_mut()
            .query::<&HoistSession>()
            .iter(app.world())
            .count(),
        1
    );
}

#[test]
fn unmapping_root_during_family_reclaim_keeps_the_surviving_placeholder() {
    let mut app = test_app();
    let root_surface = SurfaceId::new(96);
    let child_surface = SurfaceId::new(97);
    let root = map_server_decorated_surface(&mut app, root_surface);
    let child = map_server_decorated_surface(&mut app, child_surface);
    set_toplevel_parent(&mut app, child_surface, Some(root_surface));
    app.world_mut().write_message(HoistWindow { window: root });
    app.update();
    let root_session = app
        .world_mut()
        .query::<(Entity, &HoistSession)>()
        .iter(app.world())
        .find_map(|(entity, session)| (session.source() == root).then_some(entity))
        .expect("root should have a hoist session");
    app.world_mut().write_message(ReclaimHoist {
        session: root_session,
    });
    app.update();

    enqueue_surface_event(
        app.world_mut(),
        HostSurfaceEvent {
            surface: root_surface,
            kind: HostSurfaceEventKind::TreeSnapshot(SurfaceTreeSnapshot {
                client_mapped: false,
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: Vec::new(),
            }),
        },
    );
    app.update();

    let roots = app
        .world_mut()
        .query::<(&WindowProjection, &HoistPresentation)>()
        .iter(app.world())
        .map(|(projection, _)| projection.window())
        .collect::<Vec<_>>();
    assert_eq!(roots, [child]);
}

#[test]
fn destroyed_preserved_member_becomes_a_dismissible_closed_tombstone() {
    let mut app = test_app();
    let root_surface = SurfaceId::new(98);
    let child_surface = SurfaceId::new(99);
    let root = map_server_decorated_surface(&mut app, root_surface);
    let child = map_server_decorated_surface(&mut app, child_surface);
    set_toplevel_parent(&mut app, child_surface, Some(root_surface));
    let child_geometry = *app
        .world()
        .get::<WindowGeometry>(child)
        .expect("child should retain source geometry");
    app.world_mut().write_message(HoistWindow { window: root });
    app.update();
    let (root_session, child_session, child_receiver) = {
        let sessions = app
            .world_mut()
            .query::<(Entity, &HoistSession)>()
            .iter(app.world())
            .map(|(entity, session)| (entity, *session))
            .collect::<Vec<_>>();
        let root_session = sessions
            .iter()
            .find_map(|(entity, session)| (session.source() == root).then_some(*entity))
            .expect("root should have a session");
        let (child_session, child_receiver) = sessions
            .iter()
            .find_map(|(entity, session)| {
                (session.source() == child).then_some((*entity, session.receiver()))
            })
            .expect("child should have a session");
        (root_session, child_session, child_receiver)
    };

    enqueue_surface_event(
        app.world_mut(),
        HostSurfaceEvent {
            surface: child_surface,
            kind: HostSurfaceEventKind::Destroyed,
        },
    );
    app.update();

    assert!(app.world().get_entity(child).is_ok());
    assert!(!app.world().entities().contains(child_receiver));
    assert_eq!(
        app.world()
            .get::<HoistSession>(child_session)
            .map(|session| session.phase),
        Some(HoistSessionPhase::Closed)
    );
    assert_eq!(
        app.world().get::<WindowGeometry>(child),
        Some(&child_geometry)
    );
    let tombstone = app
        .world()
        .get::<PrimaryWindowPresentation>(child)
        .expect("closed preserved member should retain a placeholder")
        .entity();
    assert_eq!(
        app.world()
            .get::<HoistPresentation>(tombstone)
            .map(|presentation| presentation.state),
        Some(HoistPlaceholderState::Closed)
    );
    assert_eq!(projection_count(&mut app, child), 1);
    let tombstone_children = app
        .world()
        .get::<Children>(tombstone)
        .expect("tombstone should have status and dismissal children");
    assert!(tombstone_children.iter().any(|child| {
        app.world()
            .get::<DismissHoistTombstoneHandle>(*child)
            .is_some()
    }));
    assert!(
        tombstone_children
            .iter()
            .all(|child| { app.world().get::<ReclaimHoistHandle>(*child).is_none() })
    );

    app.world_mut().write_message(ReclaimHoist {
        session: root_session,
    });
    app.update();
    commit_server_decorated_surface(&mut app, root_surface, UVec2::new(320, 240));
    app.update();
    assert!(!app.world().entities().contains(root_session));
    assert!(app.world().entities().contains(child_session));
    assert!(app.world().get::<HoistedWindow>(child).is_some());

    app.world_mut().write_message(DismissHoistTombstone {
        session: child_session,
    });
    app.update();
    assert!(!app.world().entities().contains(child_session));
    assert!(app.world().get_entity(child).is_err());
}

#[test]
fn unmapping_the_source_ends_the_local_session() {
    let mut app = test_app();
    let surface = SurfaceId::new(72);
    let source = map_server_decorated_surface(&mut app, surface);
    app.world_mut()
        .write_message(HoistWindow { window: source });
    app.update();
    let (session, receiver) = app
        .world_mut()
        .query::<(Entity, &HoistSession)>()
        .single(app.world())
        .map(|(entity, session)| (entity, session.receiver()))
        .expect("hoist should create a session");

    enqueue_surface_event(
        app.world_mut(),
        HostSurfaceEvent {
            surface,
            kind: HostSurfaceEventKind::TreeSnapshot(SurfaceTreeSnapshot {
                client_mapped: false,
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: Vec::new(),
            }),
        },
    );
    app.update();

    assert!(!app.world().entities().contains(session));
    assert!(!app.world().entities().contains(receiver));
    assert!(
        app.world()
            .get::<WindowPresentationOverride>(source)
            .is_none()
    );

    commit_server_decorated_surface(&mut app, surface, UVec2::new(320, 240));
    app.update();
    assert!(
        app.world()
            .get::<PrimaryWindowPresentation>(source)
            .is_some()
    );
    assert!(app.world().get::<HoistedWindow>(source).is_none());
}
