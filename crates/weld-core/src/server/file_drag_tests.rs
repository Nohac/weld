//! Native source offers tested through actual Wayland data-device requests.
use super::*;
use crate::{
    input::source::libinput::LibinputAdapter,
    output::{OutputConfiguration, OutputId, OutputLayout, OutputScale, OutputTopology},
    runtime::drag::FileDragPayload,
    surface::{Extent, LogicalPoint},
};
use std::io::Read;
use wayland_client::protocol::{wl_data_device, wl_data_device_manager, wl_data_offer};
use weld_client::{
    ButtonState, ClientPointerRoute, ClientPointerRouteUpdate, InputPosition, InputTransform,
    LinuxButtonCode, SurfaceLayerId,
};
use weld_input::{KeyboardKeyState, LinuxKeycode, RawSeatEvent, RawSeatEventKind};
use winit::keyboard::{Key, NamedKey};

#[derive(Default)]
pub(super) struct Probe {
    offer: Option<wl_data_offer::WlDataOffer>,
    enters: Vec<wayland_client::backend::ObjectId>,
    drops: usize,
    leaves: usize,
}
pub(super) fn device_event(probe: &mut Probe, event: &wl_data_device::Event) {
    match event {
        wl_data_device::Event::Enter {
            serial,
            surface,
            id: Some(offer),
            ..
        } => {
            offer.accept(*serial, Some("text/uri-list".into()));
            offer.set_actions(
                wl_data_device_manager::DndAction::Copy,
                wl_data_device_manager::DndAction::Copy,
            );
            probe.offer = Some(offer.clone());
            probe.enters.push(surface.id());
        }
        wl_data_device::Event::Drop => probe.drops += 1,
        wl_data_device::Event::Leave => probe.leaves += 1,
        _ => {}
    }
}

fn motion(f: &mut Fixture, target: Option<SurfaceId>) {
    let position = InputPosition::new(1.0, 1.0);
    assert!(!f.server.dispatch_file_drag_input(&RawSeatEvent::new(
        RawSeatEventKind::PointerMotion {
            position,
            relative: None
        },
        1
    )));
    f.server.publish_file_drag_route(ClientPointerRouteUpdate {
        route: target.map(|surface| ClientPointerRoute {
            surface,
            layer: SurfaceLayerId::new(1),
            transform: InputTransform::IDENTITY,
        }),
        position,
        time: 1,
    });
    f.sync();
    f.sync();
}
fn button(f: &mut Fixture, state: ButtonState) {
    assert!(f.server.dispatch_file_drag_input(&RawSeatEvent::new(
        RawSeatEventKind::PointerButton {
            position: Some(InputPosition::new(1.0, 1.0)),
            button: LinuxButtonCode(0x110),
            state,
        },
        2
    )));
    f.sync();
}

#[test]
#[ignore = "uses a private Wayland socket; run serialized"]
fn file_drag_starts_at_the_drm_cursor_before_motion_and_after_reactivation() {
    let mut f = Fixture::new();
    let output = OutputConfiguration::new(
        OutputId::new(1),
        Extent::new(1920, 1080),
        OutputScale::new(1.5).expect("scale"),
        LogicalPoint::ZERO,
        true,
        None,
    )
    .expect("output");
    let input = LibinputAdapter::new(OutputTopology::new(
        OutputLayout::new(1, vec![output]).expect("layout"),
    ));
    let files = tempfile::tempdir().expect("directory");
    let payload = FileDragPayload::from_paths(&[files.path().to_owned()]).expect("payload");
    for _ in 0..2 {
        assert!(!f.server.dispatch_file_drag_input(&input.initial_event()));
        f.server
            .begin_file_drag(payload.clone())
            .expect("initialized DRM cursor can start a drag without physical motion");
        let pointer = f.server.local_input.native.get_pointer().expect("pointer");
        assert_eq!(pointer.current_location(), (640.0, 360.0).into());
        assert!(f.server.file_drag_active());
        f.server
            .dispatch_file_drag_input(&RawSeatEvent::new(RawSeatEventKind::HostFocusLost, 1));
        assert!(!f.server.file_drag_active());
        assert!(
            f.server.begin_file_drag(payload.clone()).is_err(),
            "an inactive desktop must wait for pointer reactivation"
        );
    }
}

#[test]
#[ignore = "uses a private Wayland socket; run serialized"]
fn windowless_drag_offers_files_without_a_source_window_and_cleans_up_cancel() {
    let mut f = Fixture::new();
    let manager = f
        .observer
        .selection
        .manager
        .clone()
        .expect("data device manager");
    let _device = manager.get_data_device(&f.observer.selection.seats[0], &f.queue.handle(), ());
    let windows = [1701, 1702].map(|number| {
        let surface = f.surface(number);
        let (id, _) = f.toplevel(&surface);
        surface.attach(Some(&f.buffer()), 0, 0);
        surface.commit();
        f.sync();
        (id, surface)
    });
    let dir = tempfile::tempdir().expect("files");
    let path = dir.path().join("local file.txt");
    std::fs::write(&path, b"offered by URI").expect("file");
    let payload = FileDragPayload::from_paths(&[path]).expect("payload");
    let expected = payload.0.clone();
    assert!(
        f.server.begin_file_drag(payload.clone()).is_err(),
        "requires a local pointer"
    );
    motion(&mut f, Some(windows[0].0));
    f.server.begin_file_drag(payload.clone()).expect("begin");
    f.sync();
    f.sync();
    assert!(
        f.server.begin_file_drag(payload.clone()).is_err(),
        "one active drag"
    );
    assert_eq!(f.observer.file_drag.enters, [windows[0].1.id()]);
    motion(&mut f, Some(windows[1].0));
    assert_eq!(
        f.observer.file_drag.enters,
        [windows[0].1.id(), windows[1].1.id()]
    );
    let offer = f.observer.file_drag.offer.clone().expect("offer");
    button(&mut f, ButtonState::Pressed);
    button(&mut f, ButtonState::Released);
    assert_eq!(f.observer.file_drag.drops, 1);
    assert!(!f.server.file_drag_active());
    let (mut read, write) = UnixStream::pair().expect("transfer pipe");
    read.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    offer.receive("text/uri-list".into(), write.as_fd());
    drop(write);
    f.sync();
    let mut bytes = Vec::new();
    read.read_to_end(&mut bytes).expect("URI transfer");
    assert_eq!(bytes, expected.as_ref());
    offer.finish();
    f.sync();

    f.server.begin_file_drag(payload.clone()).expect("restart");
    f.sync();
    f.sync();
    button(&mut f, ButtonState::Pressed);
    for state in [KeyboardKeyState::Pressed, KeyboardKeyState::Released] {
        assert!(f.server.dispatch_file_drag_input(&RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(1),
                logical_key: Some(Key::Named(NamedKey::Escape)),
                state,
            },
            3
        )));
    }
    button(&mut f, ButtonState::Released);
    assert!(!f.server.file_drag_active());
    assert_eq!(
        f.observer.file_drag.drops, 1,
        "cancel cannot produce a drop"
    );
    f.server
        .begin_file_drag(payload)
        .expect("no stuck native button or grab");
    assert!(
        !f.server
            .dispatch_file_drag_input(&RawSeatEvent::new(RawSeatEventKind::HostFocusLost, 4))
    );
    assert!(!f.server.file_drag_active());
}

#[test]
#[ignore = "uses a private Wayland socket; run serialized"]
fn file_drag_crosses_per_client_seats_without_moving_keyboard_focus() {
    let mut f = Fixture::new();
    let manager = f.observer.selection.manager.clone().expect("manager");
    let _first_device =
        manager.get_data_device(&f.observer.selection.seats[0], &f.queue.handle(), ());
    let first = f.surface(1801);
    let (first_id, _) = f.toplevel(&first);
    first.attach(Some(&f.buffer()), 0, 0);
    first.commit();
    f.sync();
    f.server.route_controller_focus(None, Some(first_id));
    motion(&mut f, Some(first_id));
    let dir = tempfile::tempdir().expect("file directory");
    let payload = FileDragPayload::from_paths(&[dir.path().to_owned()]).expect("directory offer");
    f.server.begin_file_drag(payload).expect("begin");
    f.sync();
    f.sync();
    assert_eq!(f.observer.file_drag.enters, [first.id()]);

    let (server_socket, client_socket) = UnixStream::pair().expect("socket");
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
        .expect("client");
    let connection = Connection::from_socket(client_socket).expect("connection");
    let queue = connection.new_event_queue::<Observer>();
    connection.display().get_registry(&queue.handle(), ());
    // Keep the first protocol connection alive while driving a distinct client
    // through the same server and its per-client seat binding.
    let _first_connection = (
        std::mem::replace(&mut f.connection, connection),
        std::mem::replace(&mut f.queue, queue),
        std::mem::replace(&mut f.client, client),
        std::mem::take(&mut f.observer),
    );
    f.sync();
    f.sync();
    let manager = f.observer.selection.manager.clone().expect("manager");
    let _second_device =
        manager.get_data_device(&f.observer.selection.seats[0], &f.queue.handle(), ());
    let second = f.surface(1802);
    let (second_id, _) = f.toplevel(&second);
    second.attach(Some(&f.buffer()), 0, 0);
    second.commit();
    f.sync();
    motion(&mut f, Some(second_id));
    assert_eq!(f.observer.file_drag.enters, [second.id()]);
    assert_eq!(f.server.local_input.desired_focus.get(), Some(first_id));
    button(&mut f, ButtonState::Pressed);
    button(&mut f, ButtonState::Released);
    assert_eq!(f.observer.file_drag.drops, 1);
    assert_eq!(f.server.local_input.desired_focus.get(), Some(first_id));
}
