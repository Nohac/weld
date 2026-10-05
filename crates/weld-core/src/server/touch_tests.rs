//! Native protocol evidence for contacts, framing and compatibility fallback.
use super::*;
use wayland_client::protocol::wl_touch;
use weld_client::{
    ClientInputController, ClientInputTarget, InputPosition, SurfaceLayerId, TouchEvent, TouchId,
};

#[derive(Default)]
pub(super) struct Probe {
    events: Vec<(u32, wl_touch::Event)>,
}

impl Dispatch<wl_touch::WlTouch, u32> for Observer {
    fn event(
        state: &mut Self,
        _: &wl_touch::WlTouch,
        event: wl_touch::Event,
        seat: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.touch.events.push((*seat, event));
    }
}

fn peer(connection: u64) -> ClientInputController {
    ClientInputController {
        adapter: ClientSourceId::new(9),
        connection,
    }
}
fn setup(first_only: bool) -> (Fixture, [SurfaceId; 2]) {
    let mut f = Fixture::new();
    f.observer.input.first_only = first_only;
    let windows = [1201, 1202].map(|number| {
        let root = f.surface(number);
        let (id, _) = f.toplevel(&root);
        root.attach(Some(&f.buffer()), 0, 0);
        root.commit();
        f.sync();
        id
    });
    f.server.remote_input(peer(1)).expect("peer");
    f.sync();
    f.sync();
    f.sync();
    (f, windows)
}
fn bind_touch(f: &mut Fixture, seat: usize) {
    f.observer.selection.seats[seat].get_touch(&f.queue.handle(), seat as u32);
    f.sync();
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn a_late_peer_touch_binding_cannot_split_an_active_fallback_gesture() {
    let (mut f, windows) = setup(true);
    event(&mut f, 1, windows[0], down(1));
    event(&mut f, 1, windows[0], TouchEvent::Frame);
    for (registry, name, version) in std::mem::take(&mut f.observer.input.deferred) {
        let seat = registry.bind::<wayland_client::protocol::wl_seat::WlSeat, _, _>(
            name,
            version.min(10),
            &f.queue.handle(),
            name,
        );
        seat.get_touch(&f.queue.handle(), 1);
    }
    f.sync();
    f.sync();
    f.sync();
    event(&mut f, 1, windows[0], down(2));
    event(&mut f, 1, windows[0], TouchEvent::Up { id: TouchId(1) });
    event(&mut f, 1, windows[0], TouchEvent::Up { id: TouchId(2) });
    event(&mut f, 1, windows[0], TouchEvent::Frame);
    f.sync();
    assert!(f.observer.touch.events.is_empty());
    assert_eq!(f.observer.input.buttons.len(), 2);
    event(&mut f, 1, windows[0], down(3));
    event(&mut f, 1, windows[0], TouchEvent::Frame);
    f.sync();
    assert!(
        f.observer
            .touch
            .events
            .iter()
            .any(|(_, event)| matches!(event, wl_touch::Event::Down { .. }))
    );
}
fn event(f: &mut Fixture, controller: u64, surface: SurfaceId, event: TouchEvent) {
    f.server.route_touch(
        Some(peer(controller)),
        ClientInputTarget::Touch {
            surface,
            layer: SurfaceLayerId::new(1),
        },
        event,
        1,
    );
}
fn down(id: u64) -> TouchEvent {
    TouchEvent::Down {
        id: TouchId(id),
        position: InputPosition::new(1.0, 1.0),
    }
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn destroying_a_contact_subsurface_cancels_the_gesture() {
    let (mut f, _) = setup(false);
    bind_touch(&mut f, 1);
    let root = f.surface(1301);
    let (surface, _) = f.toplevel(&root);
    root.attach(Some(&f.buffer()), 0, 0);
    root.commit();
    f.sync();
    let child = f.surface(1302);
    let subsurface = f
        .observer
        .subsurfaces
        .as_ref()
        .expect("subcompositor")
        .get_subsurface(&child, &root, &f.queue.handle(), ());
    subsurface.set_desync();
    child.attach(Some(&f.buffer()), 0, 0);
    child.commit();
    root.commit();
    f.sync();
    let target = ClientInputTarget::Touch {
        surface,
        layer: SurfaceLayerId::new(2),
    };
    f.server.route_touch(Some(peer(1)), target, down(1), 1);
    f.server
        .route_touch(Some(peer(1)), target, TouchEvent::Frame, 1);
    f.sync();
    assert!(f.observer.touch.events.iter().any(|(_, event)| {
        matches!(event, wl_touch::Event::Down { surface, .. } if surface == &child)
    }));
    child.destroy();
    f.sync();
    assert!(
        f.observer
            .touch
            .events
            .iter()
            .any(|(_, event)| matches!(event, wl_touch::Event::Cancel))
    );
    // The application root survives, and can immediately receive another gesture.
    event(&mut f, 1, surface, down(2));
    event(&mut f, 1, surface, TouchEvent::Frame);
    f.sync();
    assert!(matches!(
        f.observer.touch.events.last(),
        Some((_, wl_touch::Event::Frame))
    ));
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn retirement_flushes_a_final_up_waiting_for_its_frame() {
    let (mut f, windows) = setup(false);
    bind_touch(&mut f, 1);
    event(&mut f, 1, windows[0], down(1));
    event(&mut f, 1, windows[0], TouchEvent::Frame);
    event(&mut f, 1, windows[0], TouchEvent::Up { id: TouchId(1) });
    f.server.retire_input_target(windows[0], 2);
    f.sync();
    assert!(matches!(
        f.observer.touch.events.last(),
        Some((_, wl_touch::Event::Frame))
    ));
    assert!(matches!(
        f.observer.touch.events.iter().rev().nth(1),
        Some((_, wl_touch::Event::Up { .. }))
    ));
    event(&mut f, 1, windows[1], down(2));
    event(&mut f, 1, windows[1], TouchEvent::Frame);
    f.sync();
    assert_eq!(
        f.observer
            .touch
            .events
            .iter()
            .filter(|(_, event)| matches!(event, wl_touch::Event::Down { .. }))
            .count(),
        2
    );
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn native_contacts_keep_separate_targets_and_cancel_after_a_completed_frame() {
    let (mut f, windows) = setup(false);
    bind_touch(&mut f, 1);
    event(&mut f, 1, windows[0], down(u64::MAX));
    event(&mut f, 1, windows[1], down(17));
    event(&mut f, 1, windows[0], TouchEvent::Frame);
    event(&mut f, 1, windows[1], TouchEvent::Frame);
    f.sync();
    let downs = f
        .observer
        .touch
        .events
        .iter()
        .filter_map(|(_, event)| match event {
            wl_touch::Event::Down {
                id, surface, x, y, ..
            } => Some((*id, surface.id(), *x, *y)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(downs.len(), 2);
    assert_ne!(
        downs[0].1, downs[1].1,
        "each contact starts on its own window"
    );
    assert_ne!(downs[0].0, downs[1].0);
    assert_eq!((downs[0].2, downs[0].3), (1.0, 1.0));
    assert_eq!(
        f.observer
            .touch
            .events
            .iter()
            .filter(|(_, event)| matches!(event, wl_touch::Event::Frame))
            .count(),
        1
    );
    event(
        &mut f,
        1,
        windows[0],
        TouchEvent::Motion {
            id: TouchId(u64::MAX),
            position: InputPosition::new(55.0, -3.0),
        },
    );
    event(
        &mut f,
        1,
        windows[0],
        TouchEvent::Up {
            id: TouchId(u64::MAX),
        },
    );
    event(&mut f, 1, windows[0], TouchEvent::Frame);
    event(&mut f, 1, windows[1], TouchEvent::Cancel);
    f.sync();
    assert!(f.observer.touch.events.iter().any(|(_, event)| matches!(event, wl_touch::Event::Motion { x, y, .. } if *x == 55.0 && *y == -3.0)));
    assert_eq!(
        f.observer
            .touch
            .events
            .iter()
            .filter(|(_, event)| matches!(event, wl_touch::Event::Cancel))
            .count(),
        1
    );
    assert!(
        f.observer.input.buttons.is_empty(),
        "native touch must not also emit mouse buttons"
    );
    event(&mut f, 1, windows[1], down(17));
    event(&mut f, 1, windows[1], TouchEvent::Frame);
    f.server.retire_remote_seat(peer(1));
    f.sync();
    assert_eq!(
        f.observer
            .touch
            .events
            .iter()
            .filter(|(_, event)| matches!(event, wl_touch::Event::Cancel))
            .count(),
        2
    );
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn fallback_keeps_only_the_primary_contact_and_pins_mode_until_all_fingers_lift() {
    let (mut f, windows) = setup(true);
    event(&mut f, 1, windows[0], down(1));
    event(&mut f, 1, windows[0], down(2));
    event(&mut f, 1, windows[0], TouchEvent::Frame);
    bind_touch(&mut f, 0);
    event(&mut f, 1, windows[0], TouchEvent::Up { id: TouchId(1) });
    event(&mut f, 1, windows[0], down(3));
    event(&mut f, 1, windows[0], TouchEvent::Up { id: TouchId(2) });
    event(&mut f, 1, windows[0], TouchEvent::Up { id: TouchId(3) });
    event(&mut f, 1, windows[0], TouchEvent::Frame);
    f.sync();
    assert_eq!(
        f.observer.input.buttons.len(),
        2,
        "one primary press and release; no finger promotion"
    );
    assert!(
        f.observer.touch.events.is_empty(),
        "late touch binding cannot switch an active interaction"
    );
    event(&mut f, 1, windows[0], down(1));
    event(&mut f, 1, windows[0], TouchEvent::Frame);
    event(&mut f, 1, windows[0], TouchEvent::Cancel);
    f.sync();
    assert!(
        f.observer
            .touch
            .events
            .iter()
            .any(|(_, event)| matches!(event, wl_touch::Event::Down { .. }))
    );
    assert!(
        f.observer
            .touch
            .events
            .iter()
            .any(|(_, event)| matches!(event, wl_touch::Event::Cancel))
    );
    assert_eq!(f.observer.input.buttons.len(), 2);
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn disconnect_cancels_only_its_controller_and_surface_removal_cancels_native_context() {
    let (mut f, windows) = setup(false);
    bind_touch(&mut f, 1);
    f.server.remote_input(peer(2)).expect("second peer");
    f.sync();
    f.sync();
    f.sync();
    bind_touch(&mut f, 2);
    event(&mut f, 1, windows[0], down(1));
    event(&mut f, 1, windows[0], TouchEvent::Frame);
    event(&mut f, 2, windows[1], down(1));
    event(&mut f, 2, windows[1], TouchEvent::Frame);
    f.server.retire_remote_seat(peer(1));
    f.sync();
    assert!(
        f.observer
            .touch
            .events
            .iter()
            .any(|(seat, event)| *seat == 1 && matches!(event, wl_touch::Event::Cancel))
    );
    assert!(
        !f.observer
            .touch
            .events
            .iter()
            .any(|(seat, event)| *seat == 2 && matches!(event, wl_touch::Event::Cancel))
    );
    let surface = f
        .server
        .toplevels
        .get(windows[1])
        .expect("window")
        .surface
        .wl_surface()
        .clone();
    f.server.retire_window(&surface);
    f.sync();
    assert!(
        f.observer
            .touch
            .events
            .iter()
            .any(|(seat, event)| *seat == 2 && matches!(event, wl_touch::Event::Cancel))
    );
}
