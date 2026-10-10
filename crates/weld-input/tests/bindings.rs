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
fn modes_switch_within_one_input_batch_and_preserve_held_releases_on_reload() {
    let mut app = App::new();
    app.add_plugins(GlobalShortcutPlugin);
    let mut owner = GlobalShortcutSet::default();
    let chord = |key| GlobalShortcut::new(key, GlobalShortcutModifiers::default());
    let definitions = || {
        vec![
            GlobalShortcutMode {
                name: "default".into(),
                bindings: vec![(chord(KeyCode::KeyR), Some("resize".into()))],
            },
            GlobalShortcutMode {
                name: "resize".into(),
                bindings: vec![
                    (chord(KeyCode::KeyA), None),
                    (chord(KeyCode::Escape), Some("default".into())),
                ],
            },
        ]
    };
    let ids = owner
        .replace_modes(
            &mut app.world_mut().resource_mut::<GlobalShortcutRegistry>(),
            definitions(),
        )
        .expect("valid modes");
    assert!(!filter_global_shortcut_event(
        app.world_mut(),
        &key(30, KeyboardKeyState::Pressed)
    ));
    filter_global_shortcut_event(app.world_mut(), &key(30, KeyboardKeyState::Released));
    for code in [19, 30] {
        assert!(filter_global_shortcut_event(
            app.world_mut(),
            &key(code, KeyboardKeyState::Pressed)
        ));
    }
    assert_eq!(owner.active_mode(app.world().resource()), Some("resize"));
    let mut cursor = MessageCursor::<GlobalShortcutPressed>::default();
    assert_eq!(
        cursor
            .read(app.world().resource::<Messages<GlobalShortcutPressed>>())
            .map(|event| event.shortcut())
            .collect::<Vec<_>>(),
        [ids[0][0], ids[1][0]]
    );
    owner
        .replace_modes(
            &mut app.world_mut().resource_mut::<GlobalShortcutRegistry>(),
            definitions(),
        )
        .expect("reload");
    assert_eq!(owner.active_mode(app.world().resource()), Some("resize"));
    assert!(filter_global_shortcut_event(
        app.world_mut(),
        &key(1, KeyboardKeyState::Pressed)
    ));
    assert_eq!(owner.active_mode(app.world().resource()), Some("default"));
    for code in [19, 30, 1] {
        assert!(filter_global_shortcut_event(
            app.world_mut(),
            &key(code, KeyboardKeyState::Repeated)
        ));
        assert!(filter_global_shortcut_event(
            app.world_mut(),
            &key(code, KeyboardKeyState::Released)
        ));
    }
    filter_global_shortcut_event(app.world_mut(), &key(19, KeyboardKeyState::Pressed));
    owner
        .replace_modes(
            &mut app.world_mut().resource_mut::<GlobalShortcutRegistry>(),
            vec![GlobalShortcutMode {
                name: "default".into(),
                bindings: vec![],
            }],
        )
        .expect("mode removed");
    assert_eq!(owner.active_mode(app.world().resource()), Some("default"));
    assert!(filter_global_shortcut_event(
        app.world_mut(),
        &key(19, KeyboardKeyState::Released)
    ));
}

#[test]
fn remapped_control_matches_and_consumes_repeat_and_release_across_binding_reload() {
    let mut mapper = mapper();
    let mut app = App::new();
    app.add_plugins(GlobalShortcutPlugin);
    let mut bindings = GlobalShortcutSet::default();
    bindings.replace(
        &mut app.world_mut().resource_mut::<GlobalShortcutRegistry>(),
        [GlobalShortcut::new(
            KeyCode::KeyF,
            GlobalShortcutModifiers {
                control: true,
                ..Default::default()
            },
        )],
    );
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
    bindings.replace(
        &mut app.world_mut().resource_mut::<GlobalShortcutRegistry>(),
        [],
    );
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
    bindings.replace(
        &mut app.world_mut().resource_mut::<GlobalShortcutRegistry>(),
        [GlobalShortcut::new(
            KeyCode::KeyF,
            GlobalShortcutModifiers {
                control: true,
                ..Default::default()
            },
        )],
    );
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
    let mut reader = KeyboardSettingsReader::new(app.world_mut());
    assert_eq!(reader.take(app.world()), Some(KeyboardSettings::default()));
    assert_eq!(reader.take(app.world()), None);
    let settings = KeyboardSettings {
        keymap: Some(mapper().keymap().clone()),
        legacy_repeat: LegacyKeyRepeat::Emulated,
    };
    app.insert_resource(settings.clone());
    assert_eq!(reader.take(app.world()), Some(settings));
    assert_eq!(reader.take(app.world()), None);
    app.insert_resource(KeyboardSettings::default());
    assert_eq!(reader.take(app.world()), Some(KeyboardSettings::default()));
}

#[test]
fn keyboard_settings_track_multiple_edits_between_application_updates() {
    let mut app = App::new();
    register_keyboard_settings(&mut app);
    let mut reader = KeyboardSettingsReader::new(app.world_mut());
    assert!(reader.take(app.world()).is_some());

    for legacy_repeat in [LegacyKeyRepeat::Disabled, LegacyKeyRepeat::Emulated] {
        app.world_mut()
            .resource_mut::<KeyboardSettings>()
            .legacy_repeat = legacy_repeat;
        assert_eq!(
            reader
                .take(app.world())
                .map(|settings| settings.legacy_repeat),
            Some(legacy_repeat)
        );
        assert_eq!(reader.take(app.world()), None);
    }
}

#[test]
fn equal_settings_replacement_does_not_republish_after_change_detection() {
    let mut app = App::new();
    register_keyboard_settings(&mut app);
    let mut reader = KeyboardSettingsReader::new(app.world_mut());
    let settings = KeyboardSettings {
        keymap: Some(mapper().keymap().clone()),
        legacy_repeat: LegacyKeyRepeat::Client,
    };
    app.insert_resource(settings.clone());
    assert!(reader.take(app.world()).is_some());
    app.insert_resource(settings);
    assert_eq!(reader.take(app.world()), None);
    app.update();
    assert_eq!(reader.take(app.world()), None);
}
