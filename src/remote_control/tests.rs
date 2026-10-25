use super::*;
use weld_access::AccessAuthority;
use weld_app::surface::{
    ClientDecorated, ClientSource, ClientWindowMetadata, SurfaceActionQueue, SurfaceId,
};
use weld_client::{ClientAdapterRegistration, ClientProvenance, ClientSurfaceMetadata};
use weld_input::{
    GlobalShortcut, GlobalShortcutAppExt, GlobalShortcutModifiers, KeyboardKeyState, LinuxKeycode,
    RawSeatEvent, RawSeatEventKind, filter_global_shortcut_event,
};
use weld_remote_control::Capability;
use weld_window::{
    OccupiesWindow, WindowCommand, WindowCommandKind, WindowPlugin,
    persistence::{PersistenceAction, PersistenceMatch, WindowPersistenceRequest},
};

struct NoCapture;
impl Capture for NoCapture {
    fn begin(&mut self, _: &ComposedBuffer) -> Result<Box<dyn CaptureJob>> {
        anyhow::bail!("capture unused")
    }
}

fn fixture() -> (App, ClientAdapterRegistration) {
    let (adapter, client) = weld_remote_control::registration(
        WAYLAND_CLIENT_SOURCE,
        ClientSourceId::new(3),
        || {},
        NoCapture,
    );
    let mut app = App::new();
    app.add_plugins((WindowPlugin, weld_input::GlobalShortcutPlugin))
        .init_resource::<SurfaceActionQueue>()
        .init_resource::<LocalInputBlockedSurfaces>()
        .init_resource::<LocalInteractionBindings>()
        .insert_resource(Controller(client))
        .insert_resource(SessionAccess(AccessAuthority::new(|| {})))
        .add_observer(toggle)
        .add_observer(update_capability)
        .add_observer(take_back)
        .add_observer(toggle_local)
        .add_observer(fit_to_frame)
        .add_systems(
            PreUpdate,
            (admit_launches, reconcile)
                .chain()
                .after(WindowSystems::AdmissionPolicy)
                .before(WindowSystems::PresentationRevoke),
        )
        .add_systems(
            PreUpdate,
            (sync_local_interaction, block_local_input)
                .chain()
                .after(WindowSystems::UiReconcile),
        );
    (app, adapter)
}

#[test]
fn controlled_launch_is_authorized_once_and_global_revoke_covers_delayed_windows() {
    use crate::control::launch::{LaunchPolicy, WindowLaunch};
    use std::sync::Arc;
    let (mut app, _adapter) = fixture();
    let (_, window) = occupant(&mut app, 901);
    let policy = Arc::new(LaunchPolicy {
        id: 17,
        options: weld_control::launch::LaunchOptions {
            control: true,
            ..Default::default()
        },
        permit: Some(
            app.world()
                .resource::<SessionAccess>()
                .0
                .open("test-launch".into())
                .expect("permit"),
        ),
    });
    app.world_mut()
        .entity_mut(window)
        .insert(WindowLaunch(policy.clone()));
    app.update();
    assert!(app.world().get::<ControlledWindow>(window).is_some());
    app.world_mut().trigger(ToggleControl(window));
    app.update();
    assert!(
        app.world().get::<ControlledWindow>(window).is_none(),
        "manual disable stays disabled"
    );
    app.world()
        .resource::<SessionAccess>()
        .0
        .revoke_all()
        .expect("revoke");
    app.world()
        .resource::<SessionAccess>()
        .0
        .allow_new_sessions()
        .expect("allow fresh launches");
    let (_, delayed) = occupant(&mut app, 902);
    app.world_mut()
        .entity_mut(delayed)
        .insert(WindowLaunch(policy));
    app.update();
    assert!(app.world().get::<ControlledWindow>(delayed).is_none());
}

fn occupant(app: &mut App, id: u64) -> (Entity, Entity) {
    let surface = SurfaceId::for_test(id);
    let client = app
        .world_mut()
        .spawn((
            ClientSource {
                id: surface.source(),
                provenance: ClientProvenance::Local,
            },
            ClientToplevel { surface },
            ClientDecorated,
            ClientWindowMetadata(
                ClientSurfaceMetadata::new("persistent-test".into(), "Test".into())
                    .expect("metadata"),
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
    (
        client,
        app.world().get::<OccupiesWindow>(client).expect("slot").0,
    )
}

#[test]
fn persistent_slot_retains_editable_permissions_and_global_revoke_clears_them() {
    let (mut app, _adapter) = fixture();
    let (client, window) = occupant(&mut app, 501);
    app.world_mut().trigger(WindowPersistenceRequest {
        window: Some(window),
        action: PersistenceAction::Enable(PersistenceMatch::Auto),
    });
    app.world_mut().trigger(ToggleControl(window));
    app.update();
    let original = app
        .world()
        .get::<ControlledWindow>(window)
        .expect("grant")
        .0
        .id()
        .to_owned();
    let shorthand = app
        .world()
        .get::<ControlPolicy>(window)
        .expect("policy")
        .key
        .clone();
    app.world_mut().despawn(client);
    app.update();
    assert!(app.world().get::<ControlledWindow>(window).is_none());
    assert!(app.world().get::<WindowHeaderRequested>(window).is_some());
    assert!(app.world().get::<RemoteControlPanel>(window).is_some());
    app.world_mut().trigger(ToggleCapability {
        window,
        capability: Capability::Keyboard,
    });
    app.update();
    assert!(
        !app.world()
            .get::<RemoteControlPanel>(window)
            .expect("panel")
            .0
            .keyboard
    );
    let (replacement, slot) = occupant(&mut app, 502);
    assert_eq!(slot, window);
    let grant = &app
        .world()
        .get::<ControlledWindow>(window)
        .expect("fresh grant")
        .0;
    assert_ne!(
        grant.id(),
        original,
        "replacement receives a fresh control ID"
    );
    assert_eq!(
        app.world()
            .get::<ControlPolicy>(window)
            .expect("replacement policy")
            .key,
        shorthand
    );
    assert!(!grant.capabilities().keyboard);
    app.world_mut().despawn(replacement);
    app.update();
    app.world()
        .resource::<SessionAccess>()
        .0
        .revoke_all()
        .expect("revoke");
    app.update();
    assert!(app.world().get::<ControlPolicy>(window).is_none());
    assert!(app.world().get::<WindowHeaderRequested>(window).is_none());
    assert!(app.world().get::<PersistentWindow>(window).is_some());
    app.world()
        .resource::<SessionAccess>()
        .0
        .allow_new_sessions()
        .expect("allow");
    let (_, slot) = occupant(&mut app, 503);
    assert_eq!(slot, window);
    assert!(app.world().get::<ControlledWindow>(window).is_none());
}

#[test]
fn local_interaction_preserves_remote_permissions_and_independent_viewport() {
    let (mut app, adapter) = fixture();
    let mut driver = adapter.into_parts().runtime.driver;
    let (_, window) = occupant(&mut app, 510);
    let held = app.register_global_shortcut(GlobalShortcut::held(GlobalShortcutModifiers {
        control: true,
        super_key: true,
        ..Default::default()
    }));
    app.world_mut()
        .resource_mut::<LocalInteractionBindings>()
        .0
        .push(held);
    app.world_mut().trigger(ToggleControl(window));
    app.world_mut().trigger(WindowCommand {
        window,
        kind: WindowCommandKind::Focus,
    });
    app.update();
    let key = |keycode, state| {
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(keycode),
                state,
                logical_key: None,
            },
            1,
        )
    };
    for code in [29, 125] {
        filter_global_shortcut_event(app.world_mut(), &key(code, KeyboardKeyState::Pressed));
    }
    app.update();
    assert!(app.world().get::<WindowLocalInputBlocked>(window).is_some());
    driver.drain_events(&mut weld_client::ClientEventQueue::default());
    driver.drain_effects(&mut Vec::new());
    app.update();
    assert!(app.world().get::<LocalInteractionActive>(window).is_some());
    assert!(app.world().get::<WindowLocalInputBlocked>(window).is_none());
    assert!(app.world().get::<WindowExternalControl>(window).is_some());
    let grant = &app
        .world()
        .get::<ControlledWindow>(window)
        .expect("grant")
        .0;
    assert_eq!(grant.capabilities(), Capabilities::ALL);
    let result = app.world().resource::<Controller>().0.request(
        grant.id(),
        weld_remote_control::Operation::Keys {
            keycodes: vec![30],
            pressed: true,
        },
    );
    assert!(
        result
            .err()
            .expect("paused")
            .to_string()
            .contains("local interaction")
    );
    app.world_mut().trigger(ToggleLocalInteraction(window));
    for code in [29, 125] {
        filter_global_shortcut_event(app.world_mut(), &key(code, KeyboardKeyState::Released));
    }
    app.update();
    assert!(
        app.world().get::<LocalInteractionActive>(window).is_some(),
        "toggle survives releasing the hold chord"
    );
    app.world_mut().trigger(ToggleLocalInteraction(window));
    app.update();
    assert!(app.world().get::<WindowLocalInputBlocked>(window).is_some());
    assert!(app.world().get::<LocalInteractionActive>(window).is_none());
}

#[test]
fn fitting_uses_content_extent_without_transferring_viewport_ownership() {
    use weld_app::surface::take_surface_actions;
    use weld_window::PresentsWindow;

    let (mut app, _adapter) = fixture();
    let (_, window) = occupant(&mut app, 520);
    app.world_mut().trigger(ToggleControl(window));
    app.update();
    app.world_mut().spawn((
        PresentsWindow(window),
        PresentationInsets::new(3., 27., 3., 3.),
    ));
    app.update();
    app.world_mut()
        .get_mut::<WindowGeometry>(window)
        .expect("geometry")
        .size = Vec2::new(806., 630.);
    take_surface_actions(app.world_mut());
    app.world_mut().trigger(FitToFrame(window));
    let actions = take_surface_actions(app.world_mut());
    assert_eq!(
        actions,
        vec![SurfaceAction::Resize {
            surface: SurfaceId::for_test(520),
            logical_size: UVec2::new(800, 600),
            layout: ToplevelLayout::Tiled,
            resizing: false,
            fullscreen: false,
        }]
    );
    assert!(app.world().get::<WindowExternalControl>(window).is_some());
    assert!(app.world().get::<WindowLocalInputBlocked>(window).is_some());
    app.world_mut()
        .get_mut::<WindowGeometry>(window)
        .expect("geometry")
        .size = Vec2::new(906., 730.);
    app.update();
    assert!(
        !take_surface_actions(app.world_mut())
            .iter()
            .any(|action| matches!(action, SurfaceAction::Resize { .. }))
    );
}
