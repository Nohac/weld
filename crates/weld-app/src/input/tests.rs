use bevy::{
    app::App,
    camera::{ManualTextureViewHandle, NormalizedRenderTarget},
    ecs::entity::Entity,
    input::{InputPlugin, keyboard::KeyCode, mouse::MouseButton},
    picking::pointer::PointerInput,
    prelude::{MinimalPlugins, Reflect},
};
use leafwing_input_manager::prelude::{ActionState, Actionlike, InputManagerPlugin, InputMap};

use super::{
    GlobalShortcutPlugin, InputBridgePlugin, SeatInputEffect, SeatInputEffectKind,
    VirtualTerminalShortcutPlugin, enqueue_raw_input,
    raw::{
        ButtonState, InputPosition, LinuxButtonCode, LinuxKeycode, RawSeatEvent, RawSeatEventKind,
    },
    take_host_commands, take_input_effects, take_virtual_terminal_switch_request,
};
use crate::ActiveBackend;
use weld_core::runtime::{HostCommand, OutputScaleAdjustment};
use winit::keyboard::Key;

#[derive(Actionlike, Clone, Copy, Debug, Eq, Hash, PartialEq, Reflect)]
enum TestAction {
    Activate,
    Click,
}

fn projection_test_app() -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(InputPlugin)
        .add_message::<PointerInput>()
        .add_plugins(InputBridgePlugin::new(NormalizedRenderTarget::TextureView(
            ManualTextureViewHandle(1),
        )))
        .add_plugins(InputManagerPlugin::<TestAction>::default());
    let input = app
        .world_mut()
        .spawn(
            InputMap::default()
                .with(TestAction::Activate, KeyCode::KeyF)
                .with(TestAction::Click, MouseButton::Left),
        )
        .id();
    (app, input)
}

fn shortcut_test_app() -> App {
    shortcut_test_app_for(ActiveBackend::Nested)
}

fn shortcut_test_app_for(backend: ActiveBackend) -> App {
    let mut app = App::new();
    app.insert_resource(backend)
        .add_plugins(MinimalPlugins)
        .add_plugins(InputPlugin)
        .add_message::<PointerInput>()
        .add_plugins(InputBridgePlugin::new(NormalizedRenderTarget::TextureView(
            ManualTextureViewHandle(1),
        )))
        .add_plugins(GlobalShortcutPlugin);
    app
}

fn virtual_terminal_shortcut_test_app() -> App {
    let mut app = App::new();
    app.insert_resource(ActiveBackend::Drm)
        .add_plugins(MinimalPlugins)
        .add_plugins(InputPlugin)
        .add_message::<PointerInput>()
        .add_plugins(InputBridgePlugin::new(NormalizedRenderTarget::TextureView(
            ManualTextureViewHandle(1),
        )))
        .add_plugins(VirtualTerminalShortcutPlugin);
    app
}

#[test]
fn virtual_terminal_plugin_is_inactive_without_the_drm_backend() {
    let mut app = App::new();
    app.insert_resource(ActiveBackend::Nested)
        .add_plugins(VirtualTerminalShortcutPlugin);

    assert!(
        !app.world()
            .contains_resource::<super::virtual_terminal::VirtualTerminalSwitchRequest>()
    );
}

#[test]
fn raw_keyboard_input_reaches_leafwing_in_the_same_update() {
    let (mut app, input) = projection_test_app();
    let event = RawSeatEvent::new(
        RawSeatEventKind::Keyboard {
            keycode: LinuxKeycode(33),
            logical_key: Some(Key::Character("f".into())),
            state: ButtonState::Pressed,
        },
        41,
    );
    enqueue_raw_input(app.world_mut(), event.clone());

    app.update();

    let action_state = app
        .world()
        .entity(input)
        .get::<ActionState<TestAction>>()
        .expect("Leafwing should attach action state");
    assert!(action_state.pressed(&TestAction::Activate));
    assert!(action_state.just_pressed(&TestAction::Activate));
    assert_eq!(
        take_input_effects(app.world_mut()),
        [SeatInputEffect::new(
            SeatInputEffectKind::Keyboard {
                keycode: LinuxKeycode(33),
                state: ButtonState::Pressed,
            },
            41,
        )]
    );
}

#[test]
fn global_shortcut_emits_and_consumes_trigger_pair_in_the_same_update() {
    let mut app = shortcut_test_app();
    for event in [
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(125),
                logical_key: None,
                state: ButtonState::Pressed,
            },
            10,
        ),
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(33),
                logical_key: None,
                state: ButtonState::Pressed,
            },
            11,
        ),
    ] {
        enqueue_raw_input(app.world_mut(), event);
    }

    app.update();

    assert_eq!(
        take_host_commands(app.world_mut()),
        [HostCommand::Launch {
            program: "firefox".into(),
            arguments: Vec::new(),
        }]
    );
    assert_eq!(
        take_input_effects(app.world_mut()),
        [SeatInputEffect::new(
            SeatInputEffectKind::Keyboard {
                keycode: LinuxKeycode(125),
                state: ButtonState::Pressed,
            },
            10,
        )]
    );

    for event in [
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(33),
                logical_key: None,
                state: ButtonState::Released,
            },
            12,
        ),
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(125),
                logical_key: None,
                state: ButtonState::Released,
            },
            13,
        ),
    ] {
        enqueue_raw_input(app.world_mut(), event);
    }
    app.update();

    assert_eq!(
        take_input_effects(app.world_mut()),
        [SeatInputEffect::new(
            SeatInputEffectKind::Keyboard {
                keycode: LinuxKeycode(125),
                state: ButtonState::Released,
            },
            13,
        )]
    );
}

#[test]
fn output_scale_shortcuts_are_emitted_and_consumed_only_for_drm() {
    let events = [
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(125),
                logical_key: None,
                state: ButtonState::Pressed,
            },
            10,
        ),
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(13),
                logical_key: None,
                state: ButtonState::Pressed,
            },
            11,
        ),
    ];

    let mut drm = shortcut_test_app_for(ActiveBackend::Drm);
    for event in events.iter().cloned() {
        enqueue_raw_input(drm.world_mut(), event);
    }
    drm.update();
    assert_eq!(
        take_host_commands(drm.world_mut()),
        [HostCommand::AdjustOutputScale(
            OutputScaleAdjustment::Increase
        )]
    );
    assert_eq!(
        take_input_effects(drm.world_mut()),
        [SeatInputEffect::new(
            SeatInputEffectKind::Keyboard {
                keycode: LinuxKeycode(125),
                state: ButtonState::Pressed,
            },
            10,
        )]
    );

    let mut nested = shortcut_test_app_for(ActiveBackend::Nested);
    for event in events.iter().cloned() {
        enqueue_raw_input(nested.world_mut(), event);
    }
    nested.update();
    assert!(take_host_commands(nested.world_mut()).is_empty());
    assert_eq!(
        take_input_effects(nested.world_mut()),
        [
            SeatInputEffect::new(
                SeatInputEffectKind::Keyboard {
                    keycode: LinuxKeycode(125),
                    state: ButtonState::Pressed,
                },
                10,
            ),
            SeatInputEffect::new(
                SeatInputEffectKind::Keyboard {
                    keycode: LinuxKeycode(13),
                    state: ButtonState::Pressed,
                },
                11,
            ),
        ]
    );
}

#[test]
fn drm_virtual_terminal_shortcut_emits_and_consumes_the_function_key() {
    let mut app = virtual_terminal_shortcut_test_app();
    for (keycode, time) in [(29, 10), (56, 11), (60, 12)] {
        enqueue_raw_input(
            app.world_mut(),
            RawSeatEvent::new(
                RawSeatEventKind::Keyboard {
                    keycode: LinuxKeycode(keycode),
                    logical_key: None,
                    state: ButtonState::Pressed,
                },
                time,
            ),
        );
    }

    app.update();

    assert_eq!(
        take_virtual_terminal_switch_request(app.world_mut()),
        Some(2)
    );
    assert_eq!(
        take_input_effects(app.world_mut()),
        [
            SeatInputEffect::new(
                SeatInputEffectKind::Keyboard {
                    keycode: LinuxKeycode(29),
                    state: ButtonState::Pressed,
                },
                10,
            ),
            SeatInputEffect::new(
                SeatInputEffectKind::Keyboard {
                    keycode: LinuxKeycode(56),
                    state: ButtonState::Pressed,
                },
                11,
            ),
        ]
    );

    enqueue_raw_input(
        app.world_mut(),
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(60),
                logical_key: None,
                state: ButtonState::Released,
            },
            13,
        ),
    );
    app.update();

    assert!(take_input_effects(app.world_mut()).is_empty());
}

#[test]
fn pressing_a_modifier_after_a_client_key_does_not_consume_its_release() {
    let mut app = shortcut_test_app();
    enqueue_raw_input(
        app.world_mut(),
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(33),
                logical_key: None,
                state: ButtonState::Pressed,
            },
            10,
        ),
    );
    app.update();
    assert!(take_host_commands(app.world_mut()).is_empty());
    assert_eq!(take_input_effects(app.world_mut()).len(), 1);

    enqueue_raw_input(
        app.world_mut(),
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(125),
                logical_key: None,
                state: ButtonState::Pressed,
            },
            11,
        ),
    );
    app.update();
    assert!(take_host_commands(app.world_mut()).is_empty());
    assert_eq!(take_input_effects(app.world_mut()).len(), 1);

    enqueue_raw_input(
        app.world_mut(),
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(33),
                logical_key: None,
                state: ButtonState::Released,
            },
            12,
        ),
    );
    app.update();
    assert_eq!(take_input_effects(app.world_mut()).len(), 1);
}

#[test]
fn raw_event_order_and_timestamps_survive_projection() {
    let (mut app, _) = projection_test_app();
    let events = [
        RawSeatEvent::new(
            RawSeatEventKind::PointerMotion {
                position: InputPosition::new(12.0, 34.0),
            },
            7,
        ),
        RawSeatEvent::new(
            RawSeatEventKind::PointerButton {
                position: Some(InputPosition::new(12.0, 34.0)),
                button: LinuxButtonCode(0x110),
                state: ButtonState::Pressed,
            },
            8,
        ),
        RawSeatEvent::new(RawSeatEventKind::HostFocusLost, 9),
    ];
    for event in events.iter().cloned() {
        enqueue_raw_input(app.world_mut(), event);
    }

    app.update();

    assert_eq!(
        take_input_effects(app.world_mut()),
        [
            SeatInputEffect::new(
                SeatInputEffectKind::PointerMotion {
                    position: InputPosition::new(12.0, 34.0),
                    target: None,
                },
                7,
            ),
            SeatInputEffect::new(
                SeatInputEffectKind::PointerButton {
                    position: InputPosition::new(12.0, 34.0),
                    target: None,
                    button: LinuxButtonCode(0x110),
                    state: ButtonState::Pressed,
                },
                8,
            ),
            SeatInputEffect::new(SeatInputEffectKind::HostFocusLost, 9),
        ]
    );
}

#[test]
fn host_focus_loss_releases_leafwing_inputs() {
    let (mut app, input) = projection_test_app();
    enqueue_raw_input(
        app.world_mut(),
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(33),
                logical_key: Some(Key::Character("f".into())),
                state: ButtonState::Pressed,
            },
            1,
        ),
    );
    enqueue_raw_input(
        app.world_mut(),
        RawSeatEvent::new(
            RawSeatEventKind::PointerButton {
                position: Some(InputPosition::new(10.0, 10.0)),
                button: LinuxButtonCode(0x110),
                state: ButtonState::Pressed,
            },
            1,
        ),
    );
    app.update();
    enqueue_raw_input(
        app.world_mut(),
        RawSeatEvent::new(RawSeatEventKind::HostFocusLost, 2),
    );
    app.update();
    let action_state = app
        .world()
        .entity(input)
        .get::<ActionState<TestAction>>()
        .expect("Leafwing should attach action state");
    assert!(!action_state.pressed(&TestAction::Activate));
    assert!(!action_state.pressed(&TestAction::Click));
    assert!(
        !app.world()
            .resource::<bevy::input::ButtonInput<MouseButton>>()
            .pressed(MouseButton::Left)
    );
}
