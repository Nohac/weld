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
    ApplicationInputBuffer, GlobalShortcutAction, GlobalShortcutPlugin, InputBridgePlugin,
    InputOutputTarget, PointerShortcut, PointerShortcutAppExt, PointerShortcutModifiers,
    TouchpadGesture, VirtualTerminalShortcutPlugin, enqueue_application_input_batch,
    enqueue_raw_input, filter_global_shortcut_event, filter_pointer_shortcut_event,
    filter_virtual_terminal_event,
    raw::{
        ButtonState, InputDelta, InputPosition, LinuxButtonCode, LinuxKeycode, PointerGesture,
        RawSeatEvent, RawSeatEventKind, TouchpadPinch,
    },
    take_host_commands, take_input_effects, take_virtual_terminal_switch_request,
};
use crate::ActiveBackend;
use weld_core::{
    OutputConfiguration, OutputId, OutputScale,
    runtime::{HostCommand, OutputScaleAdjustment},
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
                state: ButtonState::Pressed,
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
    let super_press = RawSeatEvent::new(
        RawSeatEventKind::Keyboard {
            keycode: LinuxKeycode(125),
            logical_key: None,
            state: ButtonState::Pressed,
        },
        10,
    );
    let trigger_press = RawSeatEvent::new(
        RawSeatEventKind::Keyboard {
            keycode: LinuxKeycode(33),
            logical_key: None,
            state: ButtonState::Pressed,
        },
        11,
    );

    assert!(enqueue_host_input(&mut app, super_press));
    assert!(!enqueue_host_input(&mut app, trigger_press.clone()));
    assert!(!enqueue_host_input(&mut app, trigger_press));
    assert_eq!(
        take_host_commands(app.world_mut()),
        [HostCommand::Launch {
            program: "firefox".into(),
            arguments: Vec::new(),
        }]
    );

    app.update();
    let action = app
        .world_mut()
        .query::<&ActionState<super::shortcuts::GlobalAction>>()
        .single(app.world())
        .expect("global shortcut should retain its Leafwing mapping");
    assert!(action.pressed(&super::shortcuts::GlobalAction::Firefox));
    assert!(take_input_effects(app.world_mut()).is_empty());

    let trigger_release = RawSeatEvent::new(
        RawSeatEventKind::Keyboard {
            keycode: LinuxKeycode(33),
            logical_key: None,
            state: ButtonState::Released,
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
                state: ButtonState::Released,
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
                state: ButtonState::Pressed,
            },
            14,
        )
    ));
    assert!(take_host_commands(app.world_mut()).is_empty());
}

#[test]
fn output_scale_shortcuts_are_enabled_only_for_drm() {
    let events = || {
        [
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
        ]
    };

    let mut drm = shortcut_test_app(ActiveBackend::Drm);
    assert!(enqueue_host_input(&mut drm, events()[0].clone()));
    assert!(!enqueue_host_input(&mut drm, events()[1].clone()));
    assert_eq!(
        take_host_commands(drm.world_mut()),
        [HostCommand::AdjustOutputScale(
            OutputScaleAdjustment::Increase
        )]
    );

    let mut nested = shortcut_test_app(ActiveBackend::Nested);
    for event in events() {
        assert!(enqueue_host_input(&mut nested, event));
    }
    assert!(take_host_commands(nested.world_mut()).is_empty());
}

#[test]
fn physical_scale_match_shortcut_is_enabled_only_for_drm() {
    let events = || {
        [
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
                    keycode: LinuxKeycode(42),
                    logical_key: None,
                    state: ButtonState::Pressed,
                },
                11,
            ),
            RawSeatEvent::new(
                RawSeatEventKind::Keyboard {
                    keycode: LinuxKeycode(32),
                    logical_key: None,
                    state: ButtonState::Pressed,
                },
                12,
            ),
        ]
    };

    let mut drm = shortcut_test_app(ActiveBackend::Drm);
    for event in events() {
        enqueue_host_input(&mut drm, event);
    }
    assert_eq!(
        take_host_commands(drm.world_mut()),
        [HostCommand::MatchOutputPhysicalScale]
    );

    let mut nested = shortcut_test_app(ActiveBackend::Nested);
    for event in events() {
        assert!(enqueue_host_input(&mut nested, event));
    }
    assert!(take_host_commands(nested.world_mut()).is_empty());
}

#[test]
fn application_global_shortcut_is_consumed_without_becoming_a_host_command() {
    let mut app = shortcut_test_app(ActiveBackend::Nested);
    for (keycode, time, forwarded) in [(125, 10, true), (42, 11, true), (24, 12, false)] {
        assert_eq!(
            enqueue_host_input(
                &mut app,
                RawSeatEvent::new(
                    RawSeatEventKind::Keyboard {
                        keycode: LinuxKeycode(keycode),
                        logical_key: None,
                        state: ButtonState::Pressed,
                    },
                    time,
                ),
            ),
            forwarded
        );
    }
    assert!(take_host_commands(app.world_mut()).is_empty());
    let mut cursor = MessageCursor::<GlobalShortcutAction>::default();
    assert_eq!(
        cursor
            .read(app.world().resource::<Messages<GlobalShortcutAction>>())
            .copied()
            .collect::<Vec<_>>(),
        [GlobalShortcutAction::ToggleOutputTopology]
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
                        state: ButtonState::Pressed,
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
                state: ButtonState::Pressed,
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
