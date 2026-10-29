//! Native protocol tests with explicit presentation completion and synthetic input.

use super::*;
use crate::input::{KeyboardKeyState, LinuxKeycode, RawSeatEvent, RawSeatEventKind};
use crate::runtime::drag::FileDragPayload;
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
};

#[derive(Default)]
pub(super) struct Probe {
    pub manager: Option<ExtSessionLockManagerV1>,
    confirmed: Vec<u32>,
    denied: Vec<u32>,
    configured: HashMap<u32, (u32, u32)>,
}

delegate_noop!(Observer: ignore ExtSessionLockManagerV1);

impl Dispatch<ExtSessionLockV1, u32> for Observer {
    fn event(
        state: &mut Self,
        _: &ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_session_lock_v1::Event::Locked => state.lock.confirmed.push(*id),
            ext_session_lock_v1::Event::Finished => state.lock.denied.push(*id),
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockSurfaceV1, u32> for Observer {
    fn event(
        state: &mut Self,
        surface: &ExtSessionLockSurfaceV1,
        event: ext_session_lock_surface_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            state.lock.configured.insert(*id, (width, height));
            surface.ack_configure(serial);
        }
    }
}

fn lock(f: &Fixture, id: u32) -> ExtSessionLockV1 {
    f.observer
        .lock
        .manager
        .as_ref()
        .expect("lock capability")
        .lock(&f.queue.handle(), id)
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn session_lock_confirms_after_all_outputs_and_isolates_input_until_unlock() {
    let mut f = Fixture::with_session_lock(true);
    let capture_source = smithay::wayland::image_capture_source::ImageCaptureSource::new();
    capture_source
        .user_data()
        .insert_if_missing(|| OutputId::new(1));
    f.server.enable_output_capture();
    assert!(
        smithay::wayland::image_copy_capture::ImageCopyCaptureHandler::capture_constraints(
            &mut f.server,
            &capture_source
        )
        .is_some()
    );
    let normal = f.surface(1100);
    let (normal_id, _) = f.toplevel(&normal);
    normal.attach(Some(&f.buffer()), 0, 0);
    normal.commit();
    f.sync();
    f.server.route_controller_focus(None, Some(normal_id));

    let drag_files = tempfile::tempdir().expect("drag directory");
    let drag = FileDragPayload::from_paths(&[drag_files.path().to_owned()]).expect("drag payload");
    f.server.dispatch_file_drag_input(&RawSeatEvent::new(
        RawSeatEventKind::PointerMotion {
            position: weld_client::InputPosition::new(1.0, 1.0),
            relative: None,
        },
        1,
    ));
    f.server
        .begin_file_drag(drag.clone())
        .expect("drag before lock");

    let owner = lock(&f, 1);
    f.sync();
    assert!(f.server.session_locked());
    assert!(
        !f.server.file_drag_active(),
        "locking cancels the desktop drag"
    );
    assert!(
        f.server.begin_file_drag(drag).is_err(),
        "locked desktops reject file offers"
    );
    assert!(
        smithay::wayland::image_copy_capture::ImageCopyCaptureHandler::capture_constraints(
            &mut f.server,
            &capture_source
        )
        .is_none()
    );
    let generation = f.server.lock_generation();
    assert!(f.observer.lock.confirmed.is_empty());
    let denied = lock(&f, 2);
    f.sync();
    assert_eq!(f.observer.lock.denied, [2]);
    denied.destroy();

    let surface = f.surface(1101);
    let output = f
        .observer
        .output_objects
        .iter()
        .find(|output| f.observer.outputs[&output.id().protocol_id()] == "test-1")
        .expect("output");
    let lock_surface = owner.get_lock_surface(&surface, output, &f.queue.handle(), 1101);
    f.sync();
    assert_eq!(f.observer.lock.configured[&1101], (800, 600));
    let mut file = File::from(memfd_create("lock-test", MemfdFlags::CLOEXEC).expect("memfd"));
    file.write_all(&vec![0x80; 800 * 600 * 4]).expect("pixels");
    let pool = f.observer.shm.as_ref().expect("shm").create_pool(
        file.as_fd(),
        800 * 600 * 4,
        &f.queue.handle(),
        (),
    );
    let buffer = pool.create_buffer(
        0,
        800,
        600,
        800 * 4,
        wl_shm::Format::Xrgb8888,
        &f.queue.handle(),
        (),
    );
    pool.destroy();
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    f.sync();
    let images = f.server.lock_images();
    assert_eq!(images[&OutputId::new(1)].layers[0].image.width, 800);
    assert_eq!(
        f.server.toplevels.values().count(),
        1,
        "locker stays outside ordinary windows"
    );
    f.server.lock_frame_presented(OutputId::new(1), None);
    f.server.lock_frame_presented(OutputId::new(1), generation);
    f.sync();
    assert!(f.observer.lock.confirmed.is_empty());
    f.server.lock_frame_presented(OutputId::new(2), generation);
    f.sync();
    assert_eq!(f.observer.lock.confirmed, [1]);

    f.server.route_controller_focus(None, Some(normal_id));
    let before = f.observer.input.keys.len();
    f.server.route_controller_input(
        None,
        super::input_seat_tests::key(normal_id, KeyboardKeyState::Pressed),
    );
    f.sync();
    assert_eq!(
        f.observer.input.keys.len(),
        before,
        "ordinary route is blocked"
    );
    let key = RawSeatEvent::new(
        RawSeatEventKind::Keyboard {
            keycode: LinuxKeycode(30),
            logical_key: None,
            state: KeyboardKeyState::Pressed,
        },
        10,
    );
    let key = f.server.resolve_input(key);
    assert!(f.server.dispatch_lock_input(&key));
    f.sync();
    assert!(f.observer.input.keys.len() > before, "locker receives keys");
    let native = f
        .server
        .input_bindings
        .iter()
        .find(|binding| binding.published.is_none())
        .expect("binding");
    let focus = native
        .input
        .native
        .get_keyboard()
        .expect("keyboard")
        .current_focus()
        .expect("lock focus");
    let super::super::keyboard_focus::KeyboardFocus::Wayland(focused) = focus else {
        panic!("Wayland lock focus");
    };
    assert_eq!(
        smithay::reexports::wayland_server::Resource::id(&focused).protocol_id(),
        surface.id().protocol_id()
    );
    let before = f.observer.input.buttons.len();
    for state in [
        crate::input::ButtonState::Pressed,
        crate::input::ButtonState::Released,
    ] {
        assert!(f.server.dispatch_lock_input(&RawSeatEvent::new(
            RawSeatEventKind::PointerButton {
                position: Some(crate::input::InputPosition::new(100.0, 100.0)),
                button: crate::input::LinuxButtonCode(272),
                state,
            },
            11
        )));
    }
    f.sync();
    assert_eq!(f.observer.input.buttons.len(), before + 2);

    owner.unlock_and_destroy();
    f.sync();
    assert!(!f.server.session_locked());
    assert!(
        smithay::wayland::image_copy_capture::ImageCopyCaptureHandler::capture_constraints(
            &mut f.server,
            &capture_source
        )
        .is_some()
    );
    assert!(f.server.take_lock_focus_restore());
    f.server.route_controller_focus(None, Some(normal_id));
    assert!(!f.server.dispatch_lock_input(&key));
    lock_surface.destroy();
    surface.destroy();
    f.sync();
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn abandoned_lock_stays_protected_and_replacement_needs_fresh_frames() {
    let mut f = Fixture::with_session_lock(true);
    let owner = lock(&f, 1);
    f.sync();
    let old = f.server.lock_generation();
    owner.destroy();
    f.sync();
    assert!(f.server.session_locked());
    assert!(f.server.take_lock_redraw());
    assert!(f.server.lock_images().is_empty());
    let replacement = lock(&f, 2);
    f.sync();
    let current = f.server.lock_generation();
    assert_ne!(old, current);
    for output in [1, 2] {
        f.server.lock_frame_presented(OutputId::new(output), old);
    }
    f.sync();
    assert!(f.observer.lock.confirmed.is_empty());
    for output in [1, 2] {
        f.server
            .lock_frame_presented(OutputId::new(output), current);
    }
    f.sync();
    assert_eq!(f.observer.lock.confirmed, [2]);
    replacement.unlock_and_destroy();
    f.sync();
    assert!(!f.server.session_locked());
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn presentation_free_host_does_not_advertise_session_lock() {
    let f = Fixture::new();
    assert!(f.observer.lock.manager.is_none());
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn session_lock_preserves_xkb_remappings_and_local_led_state() {
    let mut f = Fixture::with_session_lock(true);
    let keymap = crate::input::KeyboardKeymap::compile(&crate::input::KeymapConfig {
        options: "ctrl:swapcaps,altwin:swap_lalt_lwin".into(),
        ..Default::default()
    })
    .expect("keymap");
    f.server
        .synchronize_keyboard_settings(Some(crate::input::KeyboardSettings {
            keymap: Some(keymap),
            ..Default::default()
        }))
        .expect("settings");
    let owner = lock(&f, 1);
    f.sync();
    let surface = f.surface(1300);
    let _lock_surface = owner.get_lock_surface(
        &surface,
        &f.observer.output_objects[0],
        &f.queue.handle(),
        1300,
    );
    f.sync();
    for (code, control, alt, logo, caps) in [
        (58, true, false, false, false),
        (125, false, true, false, false),
        (56, false, false, true, false),
        (29, false, false, false, true),
    ] {
        for state in [KeyboardKeyState::Pressed, KeyboardKeyState::Released] {
            let event = f.server.resolve_input(RawSeatEvent::new(
                RawSeatEventKind::Keyboard {
                    keycode: LinuxKeycode(code),
                    logical_key: None,
                    state,
                },
                1,
            ));
            assert!(f.server.dispatch_lock_input(&event));
            let keyboard = f
                .server
                .input_bindings
                .iter()
                .find(|binding| binding.published.is_none())
                .expect("local binding")
                .input
                .native
                .get_keyboard()
                .expect("keyboard");
            let modifiers = keyboard.modifier_state();
            let pressed = state == KeyboardKeyState::Pressed;
            assert_eq!(modifiers.ctrl, control && pressed);
            assert_eq!(modifiers.alt, alt && pressed);
            assert_eq!(modifiers.logo, logo && pressed);
            assert_eq!(modifiers.caps_lock, caps);
            assert_eq!(f.server.keyboard_leds().caps, Some(caps));
        }
    }
    let peer = weld_client::ClientInputController {
        adapter: ClientSourceId::new(90),
        connection: 1,
    };
    let remote = f.server.remote_input(peer).expect("remote seat");
    assert!(
        !remote
            .native
            .get_keyboard()
            .expect("remote keyboard")
            .modifier_state()
            .caps_lock
    );
    assert_eq!(
        f.server.keyboard_leds().caps,
        Some(true),
        "remote state cannot replace local keyboard LEDs"
    );
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn remote_input_and_frame_callbacks_continue_while_desktop_is_locked() {
    let mut f = Fixture::with_session_lock(true);
    let surface = f.surface(1200);
    let (id, _) = f.toplevel(&surface);
    surface.attach(Some(&f.buffer()), 0, 0);
    surface.frame(&f.queue.handle(), true);
    surface.commit();
    f.sync();
    let peer = weld_client::ClientInputController {
        adapter: ClientSourceId::new(99),
        connection: 1,
    };
    f.server.remote_input(peer).expect("remote seat");
    f.sync();
    f.sync();
    f.server.apply_presentation_claim(
        peer.adapter,
        ClientPresentationUpdate {
            surface: id,
            claim: ClientPresentationClaim::Active {
                rate: Some(PresentationRate::HZ_60),
            },
        },
    );
    let owner = lock(&f, 1);
    f.sync();
    assert!(f.server.session_locked());
    f.server.route_controller_focus(Some(peer), Some(id));
    for state in [KeyboardKeyState::Pressed, KeyboardKeyState::Released] {
        f.server
            .route_controller_input(Some(peer), super::input_seat_tests::key(id, state));
    }
    f.server.service_independent_callbacks(Instant::now());
    f.sync();
    assert_eq!(f.observer.input.keys.len(), 2);
    assert_eq!(f.observer.frames, 1);
    let generation = f.server.lock_generation();
    for output in [1, 2] {
        f.server
            .lock_frame_presented(OutputId::new(output), generation);
    }
    f.sync();
    owner.unlock_and_destroy();
    f.sync();
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn confirmed_locker_disconnect_keeps_session_locked() {
    let mut f = Fixture::with_session_lock(true);
    let _owner = lock(&f, 1);
    f.sync();
    let generation = f.server.lock_generation();
    for output in [1, 2] {
        f.server
            .lock_frame_presented(OutputId::new(output), generation);
    }
    f.sync();
    assert_eq!(f.observer.lock.confirmed, [1]);
    f.server.display_handle.backend_handle().kill_client(
        f.client.id(),
        smithay::reexports::wayland_server::backend::DisconnectReason::ConnectionClosed,
    );
    f.event_loop
        .dispatch(Some(Duration::ZERO), &mut f.server)
        .expect("disconnect dispatch");
    f.server.flush_clients();
    assert!(f.server.session_locked());
    assert!(f.server.take_lock_redraw());
    assert!(f.server.lock_images().is_empty());
    assert!(!f.server.take_lock_focus_restore());
}

#[test]
#[ignore = "launches swaylock against a private Wayland fixture"]
fn real_swaylock_maps_scaled_outputs_and_disconnect_keeps_protection() {
    use std::process::{Child, Command, Stdio};
    struct Locker(Child);
    impl Drop for Locker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut f = Fixture::with_session_lock(true);
    let mut locker = Locker(
        Command::new("swaylock")
            .args(["--config", "/dev/null", "--color", "123456"])
            .env("WAYLAND_DISPLAY", &f.server.socket_name)
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("private swaylock process"),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f.server.session_locked() {
        assert!(
            Instant::now() < deadline,
            "swaylock did not request locking"
        );
        assert!(
            locker.0.try_wait().expect("locker status").is_none(),
            "swaylock exited before locking"
        );
        f.sync();
    }
    let generation = f.server.lock_generation();
    for output in [1, 2] {
        f.server
            .lock_frame_presented(OutputId::new(output), generation);
    }
    while f.server.lock_images().len() != 2 {
        assert!(
            Instant::now() < deadline,
            "swaylock did not map both outputs"
        );
        assert!(
            locker.0.try_wait().expect("locker status").is_none(),
            "swaylock exited before rendering"
        );
        f.sync();
    }
    for frame in f.server.lock_images().values() {
        let image = &frame.layers[0].image;
        assert_eq!((image.width, image.height), (800, 600));
        assert_eq!(frame.layers.len(), 2, "background and indicator subsurface");
    }
    // Exercise successive indicator buffers and the callback/release loop,
    // without submitting a password to PAM.
    for _ in 0..3 {
        for output in [1, 2] {
            f.server
                .lock_frame_presented(OutputId::new(output), generation);
        }
        let previous = f.server.lock_images()[&OutputId::new(1)].layers[1]
            .image
            .clone();
        for state in [KeyboardKeyState::Pressed, KeyboardKeyState::Released] {
            let key = f.server.resolve_input(RawSeatEvent::new(
                RawSeatEventKind::Keyboard {
                    keycode: LinuxKeycode(30),
                    logical_key: None,
                    state,
                },
                10,
            ));
            assert!(f.server.dispatch_lock_input(&key));
        }
        while std::rc::Rc::ptr_eq(
            &previous,
            &f.server.lock_images()[&OutputId::new(1)].layers[1].image,
        ) {
            assert!(
                Instant::now() < deadline,
                "typing failed to update the indicator"
            );
            f.sync();
        }
        assert!(f.server.take_lock_redraw());
        assert!(
            f.server.lock_images()[&OutputId::new(1)].layers[1]
                .image
                .pixels
                .iter()
                .any(|byte| *byte != 0)
        );
    }
    drop(locker);
    for _ in 0..3 {
        f.sync();
    }
    assert!(f.server.session_locked());
    assert!(f.server.take_lock_redraw());
    assert!(f.server.lock_images().is_empty());
}
