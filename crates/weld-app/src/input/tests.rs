use bevy::{
    app::{App, Update},
    camera::{ManualTextureViewHandle, NormalizedRenderTarget},
    ecs::{
        entity::Entity,
        message::{MessageCursor, MessageReader, Messages},
        resource::Resource,
        system::ResMut,
    },
    input::{
        InputPlugin,
        keyboard::KeyCode,
        mouse::{MouseButton, MouseMotion},
    },
    picking::pointer::PointerInput,
    prelude::{MinimalPlugins, Reflect},
};
use leafwing_input_manager::prelude::{ActionState, Actionlike, InputManagerPlugin, InputMap};
use winit::keyboard::Key;

use super::{
    ApplicationInputBuffer, GlobalShortcut, GlobalShortcutAppExt, GlobalShortcutModifiers,
    GlobalShortcutPlugin, GlobalShortcutPressed, GlobalShortcutSet, InputBridgePlugin,
    InputOutputTarget, PointerShortcut, PointerShortcutAppExt, PointerShortcutModifiers,
    TouchpadGesture, VirtualTerminalShortcutPlugin, enqueue_application_input_batch,
    enqueue_raw_input, filter_global_shortcut_event, filter_pointer_shortcut_event,
    filter_virtual_terminal_event,
    raw::{
        ButtonState, InputDelta, InputPosition, LinuxButtonCode, LinuxKeycode, PointerGesture,
        RawSeatEvent, RawSeatEventKind, TouchpadPinch,
    },
    shell_commands::take_commands as take_host_commands,
    take_input_effects, take_virtual_terminal_switch_request,
};
use crate::ActiveBackend;
use weld_core::{
    OutputConfiguration, OutputId, OutputScale,
    surface::{Extent, LogicalPoint},
};

#[derive(Actionlike, Clone, Copy, Debug, Eq, Hash, PartialEq, Reflect)]
enum TestAction {
    Activate,
    Click,
}

#[derive(Default, Resource)]
struct CapturedTouchpadGestures(Vec<TouchpadGesture>);

fn capture_touchpad_gestures(
    mut gestures: MessageReader<TouchpadGesture>,
    mut captured: ResMut<CapturedTouchpadGestures>,
) {
    captured.0.extend(gestures.read().copied());
}

fn input_targets() -> Vec<InputOutputTarget> {
    vec![InputOutputTarget {
        configuration: OutputConfiguration::new(
            OutputId::new(1),
            Extent::new(800, 600),
            OutputScale::default(),
            LogicalPoint::ZERO,
            true,
            None,
        )
        .expect("valid test output"),
        target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
    }]
}

fn projection_test_app() -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(InputPlugin)
        .add_message::<PointerInput>()
        .add_plugins(InputBridgePlugin::new(input_targets()))
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

fn shortcut_test_app(backend: ActiveBackend) -> App {
    let mut app = App::new();
    app.insert_resource(backend)
        .add_plugins(MinimalPlugins)
        .add_plugins(InputPlugin)
        .add_message::<PointerInput>()
        .add_plugins(InputBridgePlugin::new(input_targets()))
        .add_plugins(GlobalShortcutPlugin);
    app
}

#[test]
fn upstream_repeats_preserve_bevy_held_state_without_new_presses() {
    let mut app = shortcut_test_app(ActiveBackend::Nested);
    let event = |state| {
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(30),
                logical_key: None,
                state,
            },
            10,
        )
    };
    enqueue_host_input(&mut app, event(weld_client::KeyboardKeyState::Pressed));
    app.update();
    assert!(
        app.world()
            .resource::<bevy::input::ButtonInput<KeyCode>>()
            .pressed(KeyCode::KeyA)
    );
    enqueue_host_input(&mut app, event(weld_client::KeyboardKeyState::Repeated));
    app.update();
    let keys = app.world().resource::<bevy::input::ButtonInput<KeyCode>>();
    assert!(keys.pressed(KeyCode::KeyA));
    assert!(!keys.just_pressed(KeyCode::KeyA));
    enqueue_host_input(&mut app, event(weld_client::KeyboardKeyState::Released));
    app.update();
    assert!(
        !app.world()
            .resource::<bevy::input::ButtonInput<KeyCode>>()
            .pressed(KeyCode::KeyA)
    );
}

#[test]
fn shortcut_repeats_are_consumed_without_retriggering_or_buffering() {
    let mut app = shortcut_test_app(ActiveBackend::Nested);
    app.register_global_shortcut(GlobalShortcut::new(
        KeyCode::KeyF,
        GlobalShortcutModifiers::super_key(),
    ));
    let mut buffer = ApplicationInputBuffer::default();
    let event = |keycode, state| {
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(keycode),
                logical_key: None,
                state,
            },
            10,
        )
    };
    assert!(buffer.enqueue(
        app.world_mut(),
        event(125, weld_client::KeyboardKeyState::Pressed)
    ));
    assert!(!buffer.enqueue(
        app.world_mut(),
        event(33, weld_client::KeyboardKeyState::Pressed)
    ));
    assert_eq!(
        app.world()
            .resource::<Messages<GlobalShortcutPressed>>()
            .len(),
        1
    );
    app.world_mut()
        .resource_mut::<Messages<GlobalShortcutPressed>>()
        .clear();
    assert!(!buffer.enqueue(
        app.world_mut(),
        event(33, weld_client::KeyboardKeyState::Repeated)
    ));
    assert!(
        app.world()
            .resource::<Messages<GlobalShortcutPressed>>()
            .is_empty()
    );
    assert_eq!(buffer.len(), 2);
    assert!(!buffer.enqueue(
        app.world_mut(),
        event(33, weld_client::KeyboardKeyState::Released)
    ));
}

fn enqueue_host_input(app: &mut App, event: RawSeatEvent) -> bool {
    let consumed = filter_global_shortcut_event(app.world_mut(), &event)
        | filter_virtual_terminal_event(app.world_mut(), &event)
        | filter_pointer_shortcut_event(app.world_mut(), &event);
    enqueue_raw_input(app.world_mut(), event);
    !consumed
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
fn raw_keyboard_input_reaches_leafwing_on_the_next_frame() {
    let (mut app, input) = projection_test_app();
    enqueue_raw_input(
        app.world_mut(),
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(33),
                logical_key: Some(Key::Character("f".into())),
                state: weld_client::KeyboardKeyState::Pressed,
            },
            41,
        ),
    );

    app.update();

    let action_state = app
        .world()
        .entity(input)
        .get::<ActionState<TestAction>>()
        .expect("Leafwing should attach action state");
    assert!(action_state.pressed(&TestAction::Activate));
    assert!(action_state.just_pressed(&TestAction::Activate));
    assert!(take_input_effects(app.world_mut()).is_empty());
}

#[test]
fn coalesced_pointer_motion_reports_the_aggregate_frame_delta() {
    let (mut app, _) = projection_test_app();
    let mut cursor = MessageCursor::<MouseMotion>::default();
    enqueue_raw_input(
        app.world_mut(),
        RawSeatEvent::new(
            RawSeatEventKind::PointerMotion {
                position: InputPosition::new(10.0, 20.0),
            },
            1,
        ),
    );
    app.update();
    assert_eq!(
        cursor
            .read(app.world().resource::<Messages<MouseMotion>>())
            .count(),
        0
    );

    let mut input = ApplicationInputBuffer::default();
    assert!(input.enqueue(
        app.world_mut(),
        RawSeatEvent::new(
            RawSeatEventKind::PointerMotion {
                position: InputPosition::new(20.0, 25.0),
            },
            2,
        )
    ));
    assert!(input.enqueue(
        app.world_mut(),
        RawSeatEvent::new(
            RawSeatEventKind::PointerMotion {
                position: InputPosition::new(35.0, 50.0),
            },
            3,
        )
    ));
    enqueue_application_input_batch(app.world_mut(), &mut input);
    app.update();

    let motions = cursor
        .read(app.world().resource::<Messages<MouseMotion>>())
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(motions.len(), 1);
    assert_eq!(motions[0].delta, bevy::math::Vec2::new(25.0, 30.0));
}

#[test]
fn application_buffer_retains_less_motion_without_changing_forward_decisions() {
    let mut app = shortcut_test_app(ActiveBackend::Nested);
    let mut input = ApplicationInputBuffer::default();
    let forwarded = (0..8)
        .filter(|time| {
            input.enqueue(
                app.world_mut(),
                RawSeatEvent::new(
                    RawSeatEventKind::PointerMotion {
                        position: InputPosition::new(f64::from(*time), 20.0),
                    },
                    *time,
                ),
            )
        })
        .count();

    assert_eq!(forwarded, 8);
    assert_eq!(input.len(), 1);

    app.register_pointer_shortcut(PointerShortcut::new(
        MouseButton::Left,
        PointerShortcutModifiers::default(),
    ));
    let mut captured = ApplicationInputBuffer::default();
    let events = [
        RawSeatEvent::new(
            RawSeatEventKind::PointerButton {
                position: Some(InputPosition::new(10.0, 20.0)),
                button: LinuxButtonCode(0x110),
                state: ButtonState::Pressed,
            },
            10,
        ),
        RawSeatEvent::new(
            RawSeatEventKind::PointerMotion {
                position: InputPosition::new(20.0, 20.0),
            },
            11,
        ),
        RawSeatEvent::new(
            RawSeatEventKind::PointerMotion {
                position: InputPosition::new(30.0, 20.0),
            },
            12,
        ),
        RawSeatEvent::new(
            RawSeatEventKind::PointerButton {
                position: Some(InputPosition::new(30.0, 20.0)),
                button: LinuxButtonCode(0x110),
                state: ButtonState::Released,
            },
            13,
        ),
    ];
    assert_eq!(
        events.map(|event| captured.enqueue(app.world_mut(), event)),
        [false, false, false, false]
    );
    assert_eq!(captured.len(), 3);
}

#[test]
fn global_shortcut_is_consumed_before_the_frame_and_still_buffered() {
    let mut app = shortcut_test_app(ActiveBackend::Nested);
    let shortcut = app.register_global_shortcut(GlobalShortcut::new(
        KeyCode::KeyF,
        GlobalShortcutModifiers::super_key(),
    ));
    let super_press = RawSeatEvent::new(
        RawSeatEventKind::Keyboard {
            keycode: LinuxKeycode(125),
            logical_key: None,
            state: weld_client::KeyboardKeyState::Pressed,
        },
        10,
    );
    let trigger_press = RawSeatEvent::new(
        RawSeatEventKind::Keyboard {
            keycode: LinuxKeycode(33),
            logical_key: None,
            state: weld_client::KeyboardKeyState::Pressed,
        },
        11,
    );

    assert!(enqueue_host_input(&mut app, super_press));
    assert!(!enqueue_host_input(&mut app, trigger_press.clone()));
    assert!(!enqueue_host_input(&mut app, trigger_press));
    let mut cursor = MessageCursor::<GlobalShortcutPressed>::default();
    assert_eq!(
        cursor
            .read(app.world().resource::<Messages<GlobalShortcutPressed>>())
            .map(|event| event.shortcut())
            .collect::<Vec<_>>(),
        [shortcut]
    );
    assert!(take_host_commands(app.world_mut()).is_empty());

    app.update();
    assert!(take_input_effects(app.world_mut()).is_empty());

    let trigger_release = RawSeatEvent::new(
        RawSeatEventKind::Keyboard {
            keycode: LinuxKeycode(33),
            logical_key: None,
            state: weld_client::KeyboardKeyState::Released,
        },
        12,
    );
    assert!(!enqueue_host_input(&mut app, trigger_release));
    assert!(enqueue_host_input(
        &mut app,
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(125),
                logical_key: None,
                state: weld_client::KeyboardKeyState::Released,
            },
            13,
        )
    ));
    assert!(enqueue_host_input(
        &mut app,
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(33),
                logical_key: None,
                state: weld_client::KeyboardKeyState::Pressed,
            },
            14,
        )
    ));
    assert!(take_host_commands(app.world_mut()).is_empty());
}

#[test]
fn shortcut_plugin_installs_no_launch_exit_or_scale_defaults() {
    for backend in [ActiveBackend::Nested, ActiveBackend::Drm] {
        let mut app = shortcut_test_app(backend);
        for code in [125, 42, 28, 33, 48, 13, 12, 32, 1] {
            let event = RawSeatEvent::new(
                RawSeatEventKind::Keyboard {
                    keycode: LinuxKeycode(code),
                    logical_key: None,
                    state: weld_client::KeyboardKeyState::Pressed,
                },
                1,
            );
            assert!(enqueue_host_input(&mut app, event));
        }
        assert!(take_host_commands(app.world_mut()).is_empty());
        assert!(
            app.world()
                .resource::<Messages<GlobalShortcutPressed>>()
                .is_empty()
        );
    }
}

#[test]
fn application_global_shortcut_is_consumed_without_becoming_a_host_command() {
    let mut app = shortcut_test_app(ActiveBackend::Nested);
    let shortcut = app.register_global_shortcut(GlobalShortcut::new(
        KeyCode::KeyO,
        GlobalShortcutModifiers::super_shift(),
    ));
    for (keycode, time, forwarded) in [(125, 10, true), (42, 11, true), (24, 12, false)] {
        assert_eq!(
            enqueue_host_input(
                &mut app,
                RawSeatEvent::new(
                    RawSeatEventKind::Keyboard {
                        keycode: LinuxKeycode(keycode),
                        logical_key: None,
                        state: weld_client::KeyboardKeyState::Pressed,
                    },
                    time,
                ),
            ),
            forwarded
        );
    }
    assert!(take_host_commands(app.world_mut()).is_empty());
    let mut cursor = MessageCursor::<GlobalShortcutPressed>::default();
    assert_eq!(
        cursor
            .read(app.world().resource::<Messages<GlobalShortcutPressed>>())
            .map(|pressed| pressed.shortcut())
            .collect::<Vec<_>>(),
        [shortcut]
    );
}

#[test]
fn live_shortcut_sets_replace_only_their_bindings_and_preserve_consumed_releases() {
    let mut app = shortcut_test_app(ActiveBackend::Nested);
    let independent = app.register_global_shortcut(GlobalShortcut::new(
        KeyCode::F12,
        GlobalShortcutModifiers::super_key(),
    ));
    let mut owned = GlobalShortcutSet::default();
    let old = owned
        .replace(
            app.world_mut(),
            [
                GlobalShortcut::new(KeyCode::ArrowLeft, GlobalShortcutModifiers::super_key()),
                GlobalShortcut::new(KeyCode::ArrowLeft, GlobalShortcutModifiers::super_shift()),
            ],
        )
        .expect("shortcut support");
    let key = |code, state| {
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(code),
                logical_key: None,
                state,
            },
            1,
        )
    };
    for code in [125, 42, 105] {
        filter_global_shortcut_event(
            app.world_mut(),
            &key(code, weld_client::KeyboardKeyState::Pressed),
        );
    }
    let mut cursor = MessageCursor::<GlobalShortcutPressed>::default();
    let events: Vec<_> = cursor
        .read(app.world().resource::<Messages<GlobalShortcutPressed>>())
        .map(|event| event.shortcut())
        .collect();
    assert_eq!(events, [old[1]]); // Shift must not accidentally select plain Left.
    let new = owned
        .replace(
            app.world_mut(),
            [GlobalShortcut::new(
                KeyCode::ArrowRight,
                GlobalShortcutModifiers::super_key(),
            )],
        )
        .expect("replace");
    assert!(!old.contains(&new[0]));
    assert!(filter_global_shortcut_event(
        app.world_mut(),
        &key(105, weld_client::KeyboardKeyState::Released)
    ));
    filter_global_shortcut_event(
        app.world_mut(),
        &key(42, weld_client::KeyboardKeyState::Released),
    );
    assert!(!filter_global_shortcut_event(
        app.world_mut(),
        &key(105, weld_client::KeyboardKeyState::Pressed)
    ));
    filter_global_shortcut_event(
        app.world_mut(),
        &key(105, weld_client::KeyboardKeyState::Released),
    );
    for code in [106, 88] {
        assert!(filter_global_shortcut_event(
            app.world_mut(),
            &key(code, weld_client::KeyboardKeyState::Pressed)
        ));
        assert!(filter_global_shortcut_event(
            app.world_mut(),
            &key(code, weld_client::KeyboardKeyState::Released)
        ));
    }
    let events: Vec<_> = cursor
        .read(app.world().resource::<Messages<GlobalShortcutPressed>>())
        .map(|event| event.shortcut())
        .collect();
    assert_eq!(events, [new[0], independent]);
}

#[test]
fn application_shortcut_registration_survives_later_global_plugin_setup() {
    let mut app = App::new();
    app.insert_resource(ActiveBackend::Nested)
        .add_plugins(MinimalPlugins)
        .add_plugins(InputPlugin)
        .add_message::<PointerInput>()
        .add_plugins(InputBridgePlugin::new(input_targets()));
    let shortcut = app.register_global_shortcut(GlobalShortcut::new(
        KeyCode::KeyH,
        GlobalShortcutModifiers::super_key(),
    ));
    app.add_plugins(GlobalShortcutPlugin);

    for keycode in [125, 35] {
        enqueue_host_input(
            &mut app,
            RawSeatEvent::new(
                RawSeatEventKind::Keyboard {
                    keycode: LinuxKeycode(keycode),
                    logical_key: None,
                    state: weld_client::KeyboardKeyState::Pressed,
                },
                keycode,
            ),
        );
    }

    let mut cursor = MessageCursor::<GlobalShortcutPressed>::default();
    assert_eq!(
        cursor
            .read(app.world().resource::<Messages<GlobalShortcutPressed>>())
            .map(|pressed| pressed.shortcut())
            .collect::<Vec<_>>(),
        [shortcut]
    );
}

#[test]
fn drm_virtual_terminal_shortcut_is_consumed_before_the_frame() {
    let mut app = projection_test_app().0;
    app.insert_resource(ActiveBackend::Drm)
        .add_plugins(VirtualTerminalShortcutPlugin);

    for (keycode, time, forwarded) in [(29, 10, true), (56, 11, true), (60, 12, false)] {
        assert_eq!(
            enqueue_host_input(
                &mut app,
                RawSeatEvent::new(
                    RawSeatEventKind::Keyboard {
                        keycode: LinuxKeycode(keycode),
                        logical_key: None,
                        state: weld_client::KeyboardKeyState::Pressed,
                    },
                    time,
                ),
            ),
            forwarded
        );
    }
    assert_eq!(
        take_virtual_terminal_switch_request(app.world_mut()),
        Some(2)
    );
}

#[test]
fn touchpad_gestures_remain_lossless_in_the_frame_projection() {
    let (mut app, _) = projection_test_app();
    app.init_resource::<CapturedTouchpadGestures>()
        .add_systems(Update, capture_touchpad_gestures);
    let gestures = [
        TouchpadGesture::new(
            PointerGesture::Pinch(TouchpadPinch::Begin { fingers: 2 }),
            20,
        ),
        TouchpadGesture::new(
            PointerGesture::Pinch(TouchpadPinch::Update {
                delta: InputDelta::new(1.5, -2.0),
                scale: 1.25,
                rotation: 3.0,
            }),
            21,
        ),
        TouchpadGesture::new(
            PointerGesture::Pinch(TouchpadPinch::End { cancelled: true }),
            22,
        ),
    ];
    for gesture in gestures {
        enqueue_raw_input(
            app.world_mut(),
            RawSeatEvent::new(
                RawSeatEventKind::PointerGesture {
                    gesture: gesture.gesture,
                },
                gesture.time,
            ),
        );
    }

    app.update();

    assert_eq!(
        app.world().resource::<CapturedTouchpadGestures>().0,
        gestures
    );
}

#[test]
fn host_focus_loss_releases_leafwing_inputs() {
    let (mut app, input) = projection_test_app();
    for event in [
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(33),
                logical_key: Some(Key::Character("f".into())),
                state: weld_client::KeyboardKeyState::Pressed,
            },
            1,
        ),
        RawSeatEvent::new(
            RawSeatEventKind::PointerButton {
                position: Some(InputPosition::new(10.0, 10.0)),
                button: LinuxButtonCode(0x110),
                state: ButtonState::Pressed,
            },
            1,
        ),
    ] {
        enqueue_raw_input(app.world_mut(), event);
    }
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
