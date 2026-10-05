//! Compatibility routing tested with actual Wayland bindings.
use super::input_seat_tests::{button, key};
use super::*;
use wayland_client::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_seat,
};
use weld_client::{ButtonState, ClientInputController, KeyboardKeyState};

#[derive(Default)]
pub(super) struct SelectionProbe {
    pub manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    pub seats: Vec<wl_seat::WlSeat>,
    offers: HashMap<wayland_client::backend::ObjectId, Vec<String>>,
    selected: Option<wl_data_offer::WlDataOffer>,
    cancelled: Vec<u32>,
}

fn setup() -> (Fixture, [SurfaceId; 2], ClientInputController) {
    let mut fixture = Fixture::new();
    fixture.observer.input.first_only = true;
    let windows = [801, 802].map(|number| {
        let surface = fixture.surface(number);
        let (id, _) = fixture.toplevel(&surface);
        surface.attach(Some(&fixture.buffer()), 0, 0);
        surface.commit();
        fixture.sync();
        id
    });
    let peer = ClientInputController {
        adapter: ClientSourceId::new(9),
        connection: 1,
    };
    fixture.server.remote_input(peer).expect("peer");
    fixture.sync();
    (fixture, windows, peer)
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn fallback_holds_reject_whole_conflicting_sequences_and_disconnect_releases() {
    let (mut f, windows, peer) = setup();
    f.server.route_controller_focus(None, Some(windows[0]));
    f.server
        .route_controller_input(None, key(windows[0], KeyboardKeyState::Pressed));
    f.server
        .route_controller_focus(Some(peer), Some(windows[1]));
    f.server
        .route_controller_input(Some(peer), key(windows[1], KeyboardKeyState::Pressed));
    f.server
        .route_controller_input(Some(peer), button(windows[1], ButtonState::Pressed));
    f.sync();
    assert_eq!(f.observer.input.keys.len(), 1);
    assert!(f.observer.input.buttons.is_empty());
    f.server
        .route_controller_input(None, key(windows[0], KeyboardKeyState::Released));
    // A rejected press must not produce a release or repeat after the owner lets go.
    f.server
        .route_controller_input(Some(peer), key(windows[1], KeyboardKeyState::Repeated));
    f.server
        .route_controller_input(Some(peer), key(windows[1], KeyboardKeyState::Released));
    f.server
        .route_controller_input(Some(peer), button(windows[1], ButtonState::Released));
    f.sync();
    assert_eq!(f.observer.input.keys.len(), 2);
    assert!(f.observer.input.buttons.is_empty());
    f.server
        .route_controller_input(Some(peer), key(windows[1], KeyboardKeyState::Pressed));
    f.server
        .route_controller_input(Some(peer), button(windows[1], ButtonState::Pressed));
    f.server.retire_remote_seat(peer);
    f.sync();
    assert_eq!(f.observer.input.keys.len(), 4);
    assert_eq!(f.observer.input.buttons.len(), 2);
    assert_eq!(f.server.input_bindings.len(), 1);
    assert_eq!(f.server.input_bindings[0].input.controller.get(), None);
    f.server
        .route_controller_input(None, key(windows[0], KeyboardKeyState::Pressed));
    f.server
        .route_controller_input(None, key(windows[0], KeyboardKeyState::Released));
    f.sync();
    assert_eq!(f.observer.input.keys.len(), 6);
}

fn selected_mimes(f: &Fixture) -> Vec<String> {
    let probe = &f.observer.selection;
    probe
        .selected
        .as_ref()
        .and_then(|offer| probe.offers.get(&offer.id()))
        .cloned()
        .unwrap_or_default()
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn fallback_clipboards_stay_with_controllers_after_context_moves() {
    let (mut f, windows, peer) = setup();
    let manager = f
        .observer
        .selection
        .manager
        .clone()
        .expect("data device manager");
    let device = manager.get_data_device(&f.observer.selection.seats[0], &f.queue.handle(), ());
    let local_source = manager.create_data_source(&f.queue.handle(), 1);
    local_source.offer("text/plain".into());
    f.sync();
    f.server.route_controller_focus(None, Some(windows[0]));
    device.set_selection(Some(&local_source), 0);
    f.sync();
    assert_eq!(selected_mimes(&f), ["text/plain"]);
    f.server
        .route_controller_focus(Some(peer), Some(windows[1]));
    f.sync();
    assert!(
        selected_mimes(&f).is_empty(),
        "peer starts with its own empty clipboard"
    );
    assert!(f.observer.selection.cancelled.is_empty());
    let remote_source = manager.create_data_source(&f.queue.handle(), 2);
    remote_source.offer("text/html".into());
    device.set_selection(Some(&remote_source), 0);
    f.sync();
    assert_eq!(selected_mimes(&f), ["text/html"]);
    f.server.route_controller_focus(None, Some(windows[0]));
    f.sync();
    assert_eq!(selected_mimes(&f), ["text/plain"]);
    f.server
        .route_controller_focus(Some(peer), Some(windows[1]));
    f.sync();
    local_source.destroy();
    f.sync();
    assert_eq!(
        selected_mimes(&f),
        ["text/html"],
        "destroy clears the original group only"
    );
    f.server.route_controller_focus(None, Some(windows[0]));
    f.sync();
    assert!(selected_mimes(&f).is_empty());
    f.server.retire_remote_seat(peer);
    f.sync();
    assert_eq!(f.observer.selection.cancelled, [2]);
    device.release();
    remote_source.destroy();
    f.sync();
}

delegate_noop!(Observer: ignore wl_data_device_manager::WlDataDeviceManager);
impl Dispatch<wl_data_device::WlDataDevice, ()> for Observer {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_device::Event::Selection { id } = event {
            state.selection.selected = id;
        }
    }
    wayland_client::event_created_child!(Observer, wl_data_device::WlDataDevice, [0 => (wl_data_offer::WlDataOffer, ())]);
}
impl Dispatch<wl_data_offer::WlDataOffer, ()> for Observer {
    fn event(
        state: &mut Self,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state
                .selection
                .offers
                .entry(offer.id())
                .or_default()
                .push(mime_type);
        }
    }
}
impl Dispatch<wl_data_source::WlDataSource, u32> for Observer {
    fn event(
        state: &mut Self,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_source::Event::Cancelled = event {
            state.selection.cancelled.push(*id);
        }
    }
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn late_native_binding_waits_for_the_fallback_hold_to_finish() {
    let (mut f, windows, peer) = setup();
    f.server
        .route_controller_focus(Some(peer), Some(windows[1]));
    f.server
        .route_controller_input(Some(peer), key(windows[1], KeyboardKeyState::Pressed));
    f.sync();
    let fallback = f.server.remote_inputs[&peer]
        .keyboard_binding
        .borrow()
        .clone()
        .expect("fallback");
    for (registry, name, version) in std::mem::take(&mut f.observer.input.deferred) {
        registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(10), &f.queue.handle(), name);
    }
    f.sync();
    f.sync();
    f.sync();
    f.server
        .route_controller_input(Some(peer), key(windows[1], KeyboardKeyState::Released));
    f.sync();
    assert_eq!(f.observer.input.keys.len(), 2);
    assert_eq!(
        f.observer.input.keys[0].0, f.observer.input.keys[1].0,
        "release stays on the admitting keyboard"
    );
    f.server
        .route_controller_input(Some(peer), key(windows[1], KeyboardKeyState::Pressed));
    f.server
        .route_controller_input(Some(peer), key(windows[1], KeyboardKeyState::Released));
    f.sync();
    assert_eq!(f.observer.input.keys.len(), 4);
    assert_ne!(
        f.observer.input.keys[1].0, f.observer.input.keys[2].0,
        "next sequence uses the ready native seat"
    );
    assert!(
        fallback
            .native
            .get_keyboard()
            .expect("keyboard")
            .pressed_keys()
            .is_empty()
    );
    f.server.route_controller_focus(None, Some(windows[0]));
    f.server
        .route_controller_input(None, key(windows[0], KeyboardKeyState::Pressed));
    f.server
        .route_controller_input(None, key(windows[0], KeyboardKeyState::Released));
    f.sync();
    assert_eq!(
        fallback.controller.get(),
        None,
        "desktop can reclaim the borrowed fallback after native upgrade"
    );
    assert_eq!(f.observer.input.keys.len(), 6);
    f.server.retire_remote_seat(peer);
    f.sync();
    assert_eq!(
        f.server.input_bindings.len(),
        1,
        "native peer context retired"
    );
    // Old resources may still be released after their advertised seat disappears.
    for seat in &f.observer.selection.seats {
        seat.release();
    }
    f.sync();
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn fallback_does_not_steal_another_applications_local_input_or_clipboard_focus() {
    let (mut f, windows, peer) = setup();
    let (server_socket, client_socket) = UnixStream::pair().expect("socket pair");
    let client = f
        .server
        .display_handle
        .insert_client(
            server_socket,
            Arc::new(ClientState::new(ClientId::new(
                crate::WAYLAND_CLIENT_SOURCE,
                20,
            ))),
        )
        .expect("second client");
    let connection = Connection::from_socket(client_socket).expect("connection");
    let mut queue = connection.new_event_queue::<Observer>();
    let mut observer = Observer::default();
    connection.display().get_registry(&queue.handle(), ());
    let sync = |f: &mut Fixture, queue: &mut EventQueue<Observer>, observer: &mut Observer| {
        connection.display().sync(&queue.handle(), ());
        connection.flush().expect("flush");
        f.sync();
        queue
            .prepare_read()
            .expect("read guard")
            .read()
            .expect("read");
        queue.dispatch_pending(observer).expect("events");
    };
    sync(&mut f, &mut queue, &mut observer);
    sync(&mut f, &mut queue, &mut observer);
    sync(&mut f, &mut queue, &mut observer);
    let surface = observer
        .compositor
        .as_ref()
        .expect("compositor")
        .create_surface(&queue.handle(), 900);
    let xdg =
        observer
            .shell
            .as_ref()
            .expect("shell")
            .get_xdg_surface(&surface, &queue.handle(), ());
    xdg.get_toplevel(&queue.handle(), ());
    surface.commit();
    sync(&mut f, &mut queue, &mut observer);
    let native_surface = client.object_from_protocol_id::<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>(
        &f.server.display_handle, surface.id().protocol_id()).expect("native surface");
    let second_window = f
        .server
        .toplevels
        .id_for_surface(&native_surface)
        .expect("window");
    f.server.route_controller_focus(None, Some(second_window));
    let local = f
        .server
        .local_input
        .keyboard_binding
        .borrow()
        .clone()
        .expect("local binding");
    f.server
        .route_controller_focus(Some(peer), Some(windows[1]));
    f.server
        .route_controller_input(Some(peer), key(windows[1], KeyboardKeyState::Pressed));
    f.server
        .route_controller_input(None, key(second_window, KeyboardKeyState::Pressed));
    sync(&mut f, &mut queue, &mut observer);
    assert_eq!(observer.input.keys.len(), 1);
    assert_eq!(f.observer.input.keys.len(), 1);
    assert_eq!(local.focused_toplevel.get(), Some(second_window));
    let group = f
        .server
        .local_input
        .native
        .user_data()
        .get::<super::super::seat_bindings::SelectionFocus>()
        .expect("selection focus");
    assert_eq!(
        group
            .0
            .borrow()
            .as_ref()
            .and_then(smithay::input::WeakSeat::upgrade),
        Some(local.native.clone())
    );
    f.server.retire_remote_seat(peer);
    assert_eq!(
        local
            .native
            .get_keyboard()
            .expect("keyboard")
            .pressed_keys()
            .len(),
        1
    );
    client.kill(
        &f.server.display_handle,
        smithay::reexports::wayland_server::backend::protocol::ProtocolError {
            code: 0,
            object_id: 1,
            object_interface: "wl_display".into(),
            message: "test cleanup".into(),
        },
    );
    f.sync();
    assert!(
        !f.server
            .input_bindings
            .iter()
            .any(|binding| binding.client == client)
    );
    assert!(f.server.local_input.keyboard_binding.borrow().is_none());
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn destroyed_fallback_target_releases_holds_before_another_controller_takes_over() {
    let (mut f, windows, peer) = setup();
    f.server
        .route_controller_focus(Some(peer), Some(windows[1]));
    f.server
        .route_controller_input(Some(peer), key(windows[1], KeyboardKeyState::Pressed));
    f.server
        .route_controller_input(Some(peer), button(windows[1], ButtonState::Pressed));
    let surface = f
        .server
        .toplevels
        .get(windows[1])
        .expect("window")
        .surface
        .wl_surface()
        .clone();
    f.server.retire_window(&surface);
    f.server
        .route_controller_input(Some(peer), key(windows[1], KeyboardKeyState::Released));
    f.server.route_controller_focus(None, Some(windows[0]));
    f.server
        .route_controller_input(None, key(windows[0], KeyboardKeyState::Pressed));
    f.server
        .route_controller_input(None, key(windows[0], KeyboardKeyState::Released));
    f.sync();
    assert_eq!(
        f.observer.input.keys.len(),
        4,
        "remote press and synthetic release, local press and release"
    );
    assert_eq!(f.observer.input.buttons.len(), 2);
    assert_eq!(f.server.input_bindings[0].input.controller.get(), None);
    assert_eq!(f.server.remote_inputs[&peer].desired_focus.get(), None);
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn popup_grabs_use_the_bound_context_in_native_and_fallback_routes() {
    for fallback in [false, true] {
        let mut f = Fixture::new();
        f.observer.input.first_only = fallback;
        let surface = f.surface(1001);
        let (id, parent) = f.toplevel(&surface);
        surface.attach(Some(&f.buffer()), 0, 0);
        surface.commit();
        let peer = ClientInputController {
            adapter: ClientSourceId::new(8),
            connection: 1,
        };
        f.server.remote_input(peer).expect("peer");
        f.sync();
        f.sync();
        f.sync();
        f.server.route_controller_focus(Some(peer), Some(id));
        f.server
            .route_controller_input(Some(peer), button(id, ButtonState::Pressed));
        f.sync();
        let serial = f.observer.input.last_button_serial.expect("pointer press");
        let seat = f
            .observer
            .selection
            .seats
            .last()
            .expect("bound seat")
            .clone();
        let shell = f.observer.shell.clone().expect("shell");
        let positioner = shell.create_positioner(&f.queue.handle(), ());
        positioner.set_size(2, 2);
        positioner.set_anchor_rect(0, 0, 2, 2);
        let popup = f.surface(1002);
        let popup_xdg = shell.get_xdg_surface(&popup, &f.queue.handle(), ());
        let role = popup_xdg.get_popup(Some(&parent), &positioner, &f.queue.handle(), ());
        role.grab(&seat, serial);
        popup.commit();
        f.sync();
        popup.attach(Some(&f.buffer()), 0, 0);
        popup.commit();
        f.sync();
        let input = f.server.remote_inputs[&peer]
            .pointer_binding
            .borrow()
            .clone()
            .expect("pointer context");
        assert!(
            input
                .popup_grab
                .borrow()
                .as_ref()
                .is_some_and(|grab| !grab.has_ended())
        );
        f.server.release_host_input(2);
        assert!(
            input
                .popup_grab
                .borrow()
                .as_ref()
                .is_some_and(|grab| !grab.has_ended()),
            "local focus loss preserves remote grab"
        );
        f.server
            .route_controller_input(Some(peer), button(id, ButtonState::Released));
        role.destroy();
        f.sync();
        assert!(
            input.popup_grab.borrow().is_none(),
            "destroyed menu releases protocol grabs"
        );
        assert!(!input.native.get_keyboard().expect("keyboard").is_grabbed());
        assert!(!input.native.get_pointer().expect("pointer").is_grabbed());
        f.server.retire_remote_seat(peer);
        f.sync();
        assert!(input.popup_grab.borrow().is_none());
        assert!(
            input
                .native
                .get_pointer()
                .is_none_or(|pointer| !pointer.is_grabbed())
        );
    }
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn passive_hover_and_late_leave_do_not_take_over_remote_fallback_focus() {
    let (mut f, windows, peer) = setup();
    f.server.route_controller_focus(None, Some(windows[0]));
    f.server
        .route_controller_focus(Some(peer), Some(windows[1]));
    for _ in 0..3 {
        f.server
            .route_controller_input(Some(peer), key(windows[1], KeyboardKeyState::Pressed));
        f.server
            .route_controller_input(Some(peer), key(windows[1], KeyboardKeyState::Released));
        let mut hover = button(windows[0], ButtonState::Pressed);
        hover.event = weld_client::InputEventKind::PointerMotion {
            position: weld_client::InputPosition::new(1.0, 1.0),
            relative: None,
        };
        f.server.route_controller_input(None, hover.clone());
        hover.event = weld_client::InputEventKind::PointerLeft {
            position: weld_client::InputPosition::new(1.0, 1.0),
        };
        f.server.route_controller_input(None, hover);
        assert_eq!(
            f.server.input_bindings[0].input.controller.get(),
            Some(peer)
        );
        assert_eq!(
            f.server.input_bindings[0].input.focused_toplevel.get(),
            Some(windows[1])
        );
    }
    f.server
        .route_controller_input(None, button(windows[0], ButtonState::Pressed));
    f.server
        .route_controller_input(None, button(windows[0], ButtonState::Released));
    f.sync();
    assert_eq!(f.server.input_bindings[0].input.controller.get(), None);
    assert_eq!(
        f.server.input_bindings[0].input.focused_toplevel.get(),
        Some(windows[0])
    );
    assert_eq!(f.observer.input.keys.len(), 6);
    assert_eq!(f.observer.input.buttons.len(), 2);
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn popup_teardown_preserves_parent_keyboard_focus_and_held_keys() {
    for held_button in [false, true] {
        let mut f = Fixture::new();
        f.observer.input.first_only = true;
        let root = f.surface(1101);
        let (owner, parent) = f.toplevel(&root);
        root.attach(Some(&f.buffer()), 0, 0);
        root.commit();
        let peer = ClientInputController {
            adapter: ClientSourceId::new(8),
            connection: 1,
        };
        f.server.remote_input(peer).expect("peer");
        f.sync();
        f.server.route_controller_focus(Some(peer), Some(owner));
        f.server
            .route_controller_input(Some(peer), key(owner, KeyboardKeyState::Pressed));
        let shell = f.observer.shell.clone().expect("shell");
        let positioner = shell.create_positioner(&f.queue.handle(), ());
        positioner.set_size(2, 2);
        positioner.set_anchor_rect(0, 0, 2, 2);
        let popup = f.surface(1102);
        let popup_xdg = shell.get_xdg_surface(&popup, &f.queue.handle(), ());
        let role = popup_xdg.get_popup(Some(&parent), &positioner, &f.queue.handle(), ());
        popup.commit();
        f.sync();
        popup.attach(Some(&f.buffer()), 0, 0);
        popup.commit();
        f.sync();
        let native_popup = f.client.object_from_protocol_id::<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>(
            &f.server.display_handle, popup.id().protocol_id()).expect("popup surface");
        let popup_id = f
            .server
            .popups
            .id_for_surface(&native_popup)
            .expect("popup id");
        let mut hover = button(popup_id, ButtonState::Pressed);
        hover.event = weld_client::InputEventKind::PointerMotion {
            position: weld_client::InputPosition::new(1.0, 1.0),
            relative: None,
        };
        f.server.route_controller_input(Some(peer), hover);
        if held_button {
            f.server
                .route_controller_input(Some(peer), button(popup_id, ButtonState::Pressed));
        }
        let input = f.server.remote_inputs[&peer]
            .keyboard_binding
            .borrow()
            .clone()
            .expect("input");
        let keyboard = input.native.get_keyboard().expect("keyboard");
        let focus = keyboard.current_focus();
        assert!(focus.is_some());
        role.destroy();
        f.sync();
        assert_eq!(
            keyboard.current_focus(),
            focus,
            "popup teardown preserves parent keyboard focus"
        );
        assert_eq!(keyboard.pressed_keys().len(), 1, "parent key remains held");
        assert_eq!(input.focused_toplevel.get(), Some(owner));
        assert_eq!(
            f.observer.input.keys.len(),
            1,
            "no synthetic parent key release"
        );
        assert!(
            input
                .native
                .get_pointer()
                .expect("pointer")
                .current_focus()
                .is_none()
        );
        f.server
            .route_controller_input(Some(peer), key(owner, KeyboardKeyState::Released));
        f.sync();
        assert_eq!(f.observer.input.keys.len(), 2);
    }
}
