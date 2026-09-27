use bevy::{
    app::App,
    ecs::message::{MessageCursor, Messages},
    input::{keyboard::KeyCode, mouse::MouseButton},
};
use weld_input::*;

fn mapper() -> KeyboardMapper {
    KeyboardMapper::new(
        KeyboardKeymap::compile(&KeymapConfig {
            options: "ctrl:swapcaps,altwin:swap_lalt_lwin".into(),
            ..Default::default()
        })
        .expect("keymap"),
    )
    .expect("mapper")
}

fn key(code: u32, state: KeyboardKeyState) -> RawSeatEvent {
    RawSeatEvent::new(
        RawSeatEventKind::Keyboard {
            keycode: LinuxKeycode(code),
            logical_key: None,
            state,
        },
        1,
    )
}

#[test]
fn remapped_control_matches_and_consumes_repeat_and_release_across_binding_reload() {
    let mut mapper = mapper();
    let mut app = App::new();
    app.add_plugins(GlobalShortcutPlugin);
    let mut bindings = GlobalShortcutSet::default();
    bindings
        .replace(
            app.world_mut(),
            [GlobalShortcut::new(
                KeyCode::KeyF,
                GlobalShortcutModifiers {
                    control: true,
                    ..Default::default()
                },
            )],
        )
        .expect("bindings");
    let caps = mapper.resolve(key(58, KeyboardKeyState::Pressed));
    assert!(caps.modifiers.expect("resolved").control);
    assert!(!filter_global_shortcut_event(app.world_mut(), &caps));
    let pressed = mapper.resolve(key(33, KeyboardKeyState::Pressed));
    assert!(filter_global_shortcut_event(app.world_mut(), &pressed));
    assert!(matches!(
        pressed.into_runtime().event,
        weld_client::RuntimeInputEventKind::Input(weld_client::InputEventKind::Keyboard {
            keycode: LinuxKeycode(33),
            state: KeyboardKeyState::Pressed
        })
    ));
    bindings.replace(app.world_mut(), []).expect("reload");
    for state in [KeyboardKeyState::Repeated, KeyboardKeyState::Released] {
        assert!(filter_global_shortcut_event(
            app.world_mut(),
            &mapper.resolve(key(33, state))
        ));
    }
    let released = mapper.resolve(key(58, KeyboardKeyState::Released));
    assert!(!released.modifiers.expect("resolved").control);
    assert!(!filter_global_shortcut_event(app.world_mut(), &released));
    assert!(!mapper.has_pressed_keys());
    let mut cursor = MessageCursor::<GlobalShortcutPressed>::default();
    assert_eq!(
        cursor
            .read(app.world().resource::<Messages<GlobalShortcutPressed>>())
            .count(),
        1
    );
    // Physical Ctrl is now Caps Lock, so it must not accidentally match Ctrl+F.
    bindings
        .replace(
            app.world_mut(),
            [GlobalShortcut::new(
                KeyCode::KeyF,
                GlobalShortcutModifiers {
                    control: true,
                    ..Default::default()
                },
            )],
        )
        .expect("bindings");
    filter_global_shortcut_event(
        app.world_mut(),
        &mapper.resolve(key(29, KeyboardKeyState::Pressed)),
    );
    assert!(!filter_global_shortcut_event(
        app.world_mut(),
        &mapper.resolve(key(33, KeyboardKeyState::Pressed))
    ));
}

#[test]
fn windows_key_becomes_alt_for_pointer_shortcuts_and_focus_loss_clears_capture() {
    let mut mapper = mapper();
    let mut app = App::new();
    app.register_pointer_shortcut(PointerShortcut::new(
        MouseButton::Left,
        PointerShortcutModifiers {
            alt: true,
            ..Default::default()
        },
    ));
    let modifier = mapper.resolve(key(125, KeyboardKeyState::Pressed));
    assert_eq!(
        modifier.modifiers,
        Some(SeatModifiers {
            alt: true,
            ..Default::default()
        })
    );
    filter_pointer_shortcut_event(app.world_mut(), &modifier);
    let button = |state| {
        RawSeatEvent::new(
            RawSeatEventKind::PointerButton {
                position: None,
                button: LinuxButtonCode(0x110),
                state,
            },
            2,
        )
    };
    assert!(filter_pointer_shortcut_event(
        app.world_mut(),
        &mapper.resolve(button(ButtonState::Pressed))
    ));
    let lost = mapper.resolve(RawSeatEvent::new(RawSeatEventKind::HostFocusLost, 3));
    assert_eq!(lost.modifiers, Some(SeatModifiers::default()));
    assert!(!mapper.has_pressed_keys());
    filter_pointer_shortcut_event(app.world_mut(), &lost);
    assert!(!filter_pointer_shortcut_event(
        app.world_mut(),
        &mapper.resolve(button(ButtonState::Released))
    ));
    assert!(!filter_pointer_shortcut_event(
        app.world_mut(),
        &mapper.resolve(button(ButtonState::Pressed))
    ));
}

#[test]
fn keyboard_settings_publish_once_and_reset_to_default_explicitly() {
    let mut app = App::new();
    register_keyboard_settings(&mut app);
    assert_eq!(
        take_keyboard_settings(app.world_mut()),
        Some(KeyboardSettings::default())
    );
    assert_eq!(take_keyboard_settings(app.world_mut()), None);
    let settings = KeyboardSettings {
        keymap: Some(mapper().keymap().clone()),
        legacy_repeat: LegacyKeyRepeat::Emulated,
    };
    app.insert_resource(settings.clone());
    assert_eq!(take_keyboard_settings(app.world_mut()), Some(settings));
    assert_eq!(take_keyboard_settings(app.world_mut()), None);
    app.insert_resource(KeyboardSettings::default());
    assert_eq!(
        take_keyboard_settings(app.world_mut()),
        Some(KeyboardSettings::default())
    );
}
