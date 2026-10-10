//! Multi-controller lifecycle exercised against real Wayland seat resources.
use super::*;
use wayland_client::{
    WEnum,
    protocol::{wl_keyboard, wl_pointer, wl_seat},
};
use weld_client::{
    ButtonState, ClientFocusRequest, ClientInputController, ClientInputEvent, ClientInputTarget,
    InputEventKind, InputPosition, KeyboardKeyState, LinuxButtonCode, LinuxKeycode, SurfaceLayerId,
};

#[derive(Default)]
pub(super) struct Probe {
    pub first_only: bool,
    pub last_button_serial: Option<u32>,
    pub deferred: Vec<(wayland_client::protocol::wl_registry::WlRegistry, u32, u32)>,
    names: HashMap<u32, String>,
    pub(super) keys: Vec<(u32, u32, wl_keyboard::KeyState)>,
    modifiers: Vec<(u32, u32, u32)>,
    pub(super) buttons: Vec<(u32, u32, wl_pointer::ButtonState)>,
}

#[test]
#[ignore = "launches a private Kitty process against the native Wayland fixture"]
fn real_kitty_shared_process_accepts_local_and_remote_input() {
    use std::{
        fs,
        process::{Child, Command, Stdio},
    };
    struct Kitty(Child);
    impl Drop for Kitty {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut f = Fixture::new();
    let directory = std::env::temp_dir().join(format!(
        "weld-kitty-seats-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    fs::create_dir_all(&directory).expect("probe directory");
    let local = directory.join("local.txt");
    let remote = directory.join("remote.txt");
    let session = directory.join("kitty.session");
    fs::write(&session, format!("os_window_title weld-local\nlaunch sh -c 'stty raw -echo; cat > {}'\nnew_os_window\nos_window_title weld-remote\nlaunch sh -c 'stty raw -echo; cat > {}'\n", local.display(), remote.display())).expect("session");
    let log = File::create(directory.join("kitty.log")).expect("log");
    let _kitty = Kitty(
        Command::new("kitty")
            .args([
                "--config",
                "NONE",
                "--override",
                "shell_integration=disabled",
                "--session",
            ])
            .arg(&session)
            .env("WAYLAND_DISPLAY", &f.server.socket_name)
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env("WAYLAND_DEBUG", "client")
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().expect("log clone")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("kitty"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while f.server.toplevels.values().count() < 2 || !local.exists() || !remote.exists() {
        assert!(
            Instant::now() < deadline,
            "Kitty startup failed; inspect {}",
            directory.display()
        );
        f.sync();
    }
    let mut windows = f
        .server
        .toplevels
        .values()
        .filter_map(|window| {
            f.server
                .toplevels
                .id_for_surface(window.surface.wl_surface())
        })
        .collect::<Vec<_>>();
    windows.sort_by_key(|surface| surface.local());
    assert_eq!(windows.len(), 2);
    assert_eq!(
        windows[0].client(),
        windows[1].client(),
        "one Wayland connection"
    );
    let peer = ClientInputController {
        adapter: ClientSourceId::new(7),
        connection: 1,
    };
    f.server.remote_input(peer).expect("peer");
    for _ in 0..10 {
        f.sync();
    }
    for (controller, surface, keycode) in [
        (None, windows[0], 30),
        (Some(peer), windows[1], 48),
        (None, windows[0], 46),
    ] {
        f.server.route_controller_focus(controller, Some(surface));
        eprintln!(
            "route {controller:?} {surface:?} bindings {}",
            f.server.input_bindings.len()
        );
        for binding in &f.server.input_bindings {
            eprintln!(
                "binding {:?} owner {:?} focus {:?} keyboards {}",
                binding.published,
                binding.input.controller.get(),
                binding.input.focused_toplevel.get(),
                binding
                    .input
                    .native
                    .get_keyboard()
                    .expect("keyboard")
                    .client_keyboards(&binding.client)
                    .count()
            );
        }
        for state in [KeyboardKeyState::Pressed, KeyboardKeyState::Released] {
            let mut event = key(surface, state);
            event.event = InputEventKind::Keyboard {
                keycode: LinuxKeycode(keycode),
                state,
            };
            f.server.route_controller_input(controller, event);
        }
        for _ in 0..10 {
            f.sync();
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while (fs::read(&local).expect("local input").len() < 2
        || fs::read(&remote).expect("remote input").is_empty())
        && Instant::now() < deadline
    {
        f.sync();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fs::read(&local).expect("local input"), b"ac");
    assert_eq!(fs::read(&remote).expect("remote input"), b"b");
    println!(
        "Kitty shared-process input evidence: {}",
        directory.display()
    );
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn first_seat_client_switches_between_local_and_remote_windows() {
    let mut f = Fixture::new();
    f.observer.input.first_only = true;
    let mut windows = Vec::new();
    for number in 701..=702 {
        let surface = f.surface(number);
        let (id, _) = f.toplevel(&surface);
        surface.attach(Some(&f.buffer()), 0, 0);
        surface.commit();
        windows.push(id);
    }
    f.sync();
    let peer = ClientInputController {
        adapter: ClientSourceId::new(7),
        connection: 1,
    };
    f.server.remote_input(peer).expect("peer");
    f.sync();
    f.sync();
    for (controller, surface) in [
        (None, windows[0]),
        (Some(peer), windows[1]),
        (None, windows[0]),
    ] {
        f.server.route_controller_focus(controller, Some(surface));
        f.server
            .route_controller_input(controller, key(surface, KeyboardKeyState::Pressed));
        f.server
            .route_controller_input(controller, key(surface, KeyboardKeyState::Released));
        f.server
            .route_controller_input(controller, button(surface, ButtonState::Pressed));
        f.server
            .route_controller_input(controller, button(surface, ButtonState::Released));
        f.sync();
    }
    assert_eq!(
        f.observer.input.names.len(),
        1,
        "client binds only the original seat"
    );
    assert_eq!(f.observer.input.keys.len(), 6);
    assert_eq!(f.observer.input.buttons.len(), 6);
}

impl Dispatch<wl_seat::WlSeat, u32> for Observer {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        id: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_seat::Event::Name { name } => {
                state.input.names.insert(*id, name);
            }
            wl_seat::Event::Capabilities {
                capabilities: WEnum::Value(capabilities),
            } => {
                if capabilities.contains(wl_seat::Capability::Keyboard) {
                    seat.get_keyboard(qh, *id);
                }
                if capabilities.contains(wl_seat::Capability::Pointer) {
                    seat.get_pointer(qh, *id);
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, u32> for Observer {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(value),
                ..
            } => state.input.keys.push((*id, key, value)),
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_locked,
                ..
            } => {
                state
                    .input
                    .modifiers
                    .push((*id, mods_depressed, mods_locked));
            }
            _ => {}
        }
    }
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn held_control_survives_closing_its_press_recipient_and_releases_on_the_next_window() {
    let mut f = Fixture::new();
    let keymap = crate::input::KeyboardKeymap::compile(&crate::input::KeymapConfig {
        options: "ctrl:swapcaps".into(),
        ..Default::default()
    })
    .expect("remapped Ctrl");
    f.server
        .synchronize_keyboard_settings(Some(crate::input::KeyboardSettings {
            keymap: Some(keymap),
            ..Default::default()
        }))
        .expect("keyboard settings");
    let windows = [901, 902].map(|number| {
        let surface = f.surface(number);
        let (id, role) = f.toplevel(&surface);
        surface.attach(Some(&f.buffer()), 0, 0);
        surface.commit();
        f.sync();
        (id, surface, role)
    });
    let input = |surface, code, state| ClientInputEvent {
        target: ClientInputTarget::Keyboard { surface },
        host_position: None,
        event: InputEventKind::Keyboard {
            keycode: LinuxKeycode(code),
            state,
        },
        time: 1,
    };
    let physical = |code, state| {
        crate::input::RawSeatEvent::new(
            crate::input::RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(code),
                logical_key: None,
                state,
            },
            1,
        )
    };
    f.server.route_controller_focus(None, Some(windows[0].0));
    for code in [58, 32] {
        f.server
            .resolve_input(physical(code, KeyboardKeyState::Pressed));
        f.server
            .route_controller_input(None, input(windows[0].0, code, KeyboardKeyState::Pressed));
    }
    // Unmapping follows the terminal's Ctrl+D exit while both keys are held.
    windows[0].1.attach(None, 0, 0);
    windows[0].1.commit();
    f.sync();
    f.server.route_controller_focus(None, Some(windows[1].0));
    f.sync();
    assert_ne!(f.observer.input.modifiers.last().expect("modifiers").1, 0);
    let binding = f
        .server
        .local_input
        .keyboard_binding
        .borrow()
        .clone()
        .expect("binding");
    let keyboard = binding.native.get_keyboard().expect("keyboard");
    assert!(keyboard.modifier_state().ctrl);
    f.server
        .resolve_input(physical(32, KeyboardKeyState::Released));
    f.server
        .resolve_input(physical(32, KeyboardKeyState::Pressed));
    f.server
        .route_controller_input(None, input(windows[1].0, 32, KeyboardKeyState::Pressed));
    assert!(keyboard.modifier_state().ctrl, "the next D retains Ctrl");

    // The original press route has gone; a raw release must still clear Ctrl.
    f.server
        .resolve_input(physical(58, KeyboardKeyState::Released));
    f.sync();
    assert!(!keyboard.modifier_state().ctrl);
    assert_eq!(
        f.observer
            .input
            .modifiers
            .last()
            .expect("released modifiers")
            .1,
        0
    );
}
#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn local_keyboard_synchronization_preserves_caps_lock_and_layout_group() {
    let mut f = Fixture::new();
    let keymap = crate::input::KeyboardKeymap::compile(&crate::input::KeymapConfig {
        layout: "us,no".into(),
        options: "grp:alt_shift_toggle".into(),
        ..Default::default()
    })
    .expect("two layouts");
    f.server
        .synchronize_keyboard_settings(Some(crate::input::KeyboardSettings {
            keymap: Some(keymap),
            ..Default::default()
        }))
        .expect("settings");
    let surface = f.surface(903);
    let (id, _) = f.toplevel(&surface);
    surface.attach(Some(&f.buffer()), 0, 0);
    surface.commit();
    f.sync();
    f.server.route_controller_focus(None, Some(id));
    let binding = f
        .server
        .local_input
        .keyboard_binding
        .borrow()
        .clone()
        .expect("binding");
    let keyboard = binding.native.get_keyboard().expect("keyboard");
    for (code, state) in [
        (58, KeyboardKeyState::Pressed),
        (58, KeyboardKeyState::Released),
        (56, KeyboardKeyState::Pressed),
        (42, KeyboardKeyState::Pressed),
        (42, KeyboardKeyState::Released),
        (56, KeyboardKeyState::Released),
    ] {
        f.server.resolve_input(crate::input::RawSeatEvent::new(
            crate::input::RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(code),
                logical_key: None,
                state,
            },
            1,
        ));
        let mut event = key(id, state);
        event.event = InputEventKind::Keyboard {
            keycode: LinuxKeycode(code),
            state,
        };
        f.server.route_controller_input(None, event);
    }
    assert!(keyboard.modifier_state().caps_lock);
    assert_eq!(keyboard.modifier_state().serialized.layout_effective, 1);
    f.server.release_seat_input(&binding, 2);
    f.server.route_controller_focus(None, Some(id));
    assert!(keyboard.modifier_state().caps_lock);
    assert_eq!(keyboard.modifier_state().serialized.layout_effective, 1);
}

impl Dispatch<wl_pointer::WlPointer, u32> for Observer {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Button {
            serial,
            button,
            state: WEnum::Value(value),
            ..
        } = event
        {
            state.input.buttons.push((*id, button, value));
            state.input.last_button_serial = Some(serial);
        }
    }
}

pub(super) fn key(surface: SurfaceId, state: KeyboardKeyState) -> ClientInputEvent {
    ClientInputEvent {
        target: ClientInputTarget::Keyboard { surface },
        host_position: None,
        event: InputEventKind::Keyboard {
            keycode: LinuxKeycode(42),
            state,
        },
        time: 1,
    }
}
pub(super) fn button(surface: SurfaceId, state: ButtonState) -> ClientInputEvent {
    ClientInputEvent {
        target: ClientInputTarget::Pointer {
            surface,
            layer: SurfaceLayerId::new(1),
        },
        host_position: None,
        event: InputEventKind::PointerButton {
            position: Some(InputPosition::new(1.0, 1.0)),
            button: LinuxButtonCode(0x110),
            state,
        },
        time: 1,
    }
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn peer_seats_share_windows_but_isolate_focus_holds_and_disconnect() {
    let mut f = Fixture::new();
    let mut windows = Vec::new();
    for number in 601..=603 {
        let surface = f.surface(number);
        let (id, _) = f.toplevel(&surface);
        surface.attach(Some(&f.buffer()), 0, 0);
        surface.commit();
        windows.push(id);
    }
    f.sync();
    let controller = |connection| ClientInputController {
        adapter: ClientSourceId::new(7),
        connection,
    };
    let first = controller(1);
    let second = controller(2);
    f.server.remote_input(first).expect("first seat");
    f.server.remote_input(second).expect("second seat");
    f.sync();
    f.sync();
    f.sync();
    assert_eq!(f.observer.input.names.len(), 3);
    f.server.focus_toplevel(Some(windows[0]));
    for (peer, surface) in [(first, windows[1]), (second, windows[2])] {
        f.server.remote_client_focus(
            peer,
            ClientFocusRequest {
                source: surface.source(),
                surface: Some(surface),
            },
        );
        f.server
            .remote_client_input(peer, key(surface, KeyboardKeyState::Pressed));
        f.server
            .remote_client_input(peer, button(surface, ButtonState::Pressed));
    }
    f.server
        .apply_client_input(key(windows[0], KeyboardKeyState::Pressed));
    f.sync();
    assert_eq!(
        f.observer
            .input
            .keys
            .iter()
            .filter(|(_, _, state)| *state == wl_keyboard::KeyState::Pressed)
            .count(),
        3
    );
    assert_eq!(
        f.observer
            .input
            .buttons
            .iter()
            .filter(|(_, _, state)| *state == wl_pointer::ButtonState::Pressed)
            .count(),
        2
    );
    assert_eq!(f.server.local_input.desired_focus.get(), Some(windows[0]));
    let first_seat = f.server.remote_inputs[&first]
        .keyboard_binding
        .borrow()
        .clone()
        .expect("first binding");
    let second_seat = f.server.remote_inputs[&second]
        .keyboard_binding
        .borrow()
        .clone()
        .expect("second binding");
    f.server.retire_remote_seat(first);
    f.sync();
    assert_eq!(f.server.remote_inputs.len(), 1);
    assert_eq!(second_seat.focused_toplevel.get(), Some(windows[2]));
    assert_eq!(
        second_seat
            .native
            .get_keyboard()
            .expect("keyboard")
            .pressed_keys()
            .len(),
        1
    );
    assert_eq!(
        f.server
            .local_input
            .keyboard_binding
            .borrow()
            .as_ref()
            .expect("local binding")
            .native
            .get_keyboard()
            .expect("local keyboard")
            .pressed_keys()
            .len(),
        1
    );
    assert_eq!(second_seat.pressed_pointer_buttons.borrow().len(), 1);
    assert!(first_seat.native.get_keyboard().is_none());
    assert!(first_seat.native.get_pointer().is_none());
    f.server.release_host_input(2);
    assert_eq!(
        second_seat
            .native
            .get_keyboard()
            .expect("keyboard")
            .pressed_keys()
            .len(),
        1
    );
    f.server
        .remote_client_input(second, button(windows[2], ButtonState::Released));
    f.server
        .remote_client_input(second, key(windows[2], KeyboardKeyState::Released));
    f.server.remote_client_focus(
        second,
        ClientFocusRequest {
            source: windows[1].source(),
            surface: Some(windows[1]),
        },
    );
    assert_eq!(
        f.server.remote_inputs.len(),
        1,
        "switching windows reuses the peer seat"
    );
    assert_eq!(second_seat.focused_toplevel.get(), Some(windows[1]));
    f.server.retire_remote_seat(second);
    f.sync();
}
