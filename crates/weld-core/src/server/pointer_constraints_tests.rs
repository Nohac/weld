//! Real Wayland requests exercise Weld's activation policy and native input path.

use super::*;
use crate::input::{InputDelta, InputPosition, RawSeatEvent, RawSeatEventKind, RelativeMotion};
use smithay::input::pointer::CursorImageStatus;
use wayland_client::protocol::{wl_pointer, wl_region};
use wayland_protocols::wp::{
    pointer_constraints::zv1::client::{
        zwp_confined_pointer_v1::{self, ZwpConfinedPointerV1},
        zwp_locked_pointer_v1::{self, ZwpLockedPointerV1},
        zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
    },
    relative_pointer::zv1::client::{
        zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
    },
};
use weld_client::{
    ClientInputEvent, ClientInputTarget, ClientPointerRoute, InputEventKind, InputTransform,
    SurfaceLayerId,
};

#[derive(Default)]
pub(super) struct Probe {
    pub manager: Option<ZwpPointerConstraintsV1>,
    pub relative_manager: Option<ZwpRelativePointerManagerV1>,
    pub pointers: HashMap<u32, wl_pointer::WlPointer>,
    pub enters: HashMap<u32, u32>,
    pub motions: Vec<(u32, f64, f64)>,
    locks: Vec<(u32, bool)>,
    confinements: Vec<(u32, bool)>,
    relative: Vec<(u32, f64, f64, f64, f64)>,
}

delegate_noop!(Observer: ignore ZwpPointerConstraintsV1);
delegate_noop!(Observer: ignore ZwpRelativePointerManagerV1);
delegate_noop!(Observer: ignore wl_region::WlRegion);

impl Dispatch<ZwpLockedPointerV1, u32> for Observer {
    fn event(
        state: &mut Self,
        _: &ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_locked_pointer_v1::Event::Locked => state.constraints.locks.push((*id, true)),
            zwp_locked_pointer_v1::Event::Unlocked => state.constraints.locks.push((*id, false)),
            _ => {}
        }
    }
}
impl Dispatch<ZwpConfinedPointerV1, u32> for Observer {
    fn event(
        state: &mut Self,
        _: &ZwpConfinedPointerV1,
        event: zwp_confined_pointer_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_confined_pointer_v1::Event::Confined => {
                state.constraints.confinements.push((*id, true))
            }
            zwp_confined_pointer_v1::Event::Unconfined => {
                state.constraints.confinements.push((*id, false))
            }
            _ => {}
        }
    }
}
impl Dispatch<ZwpRelativePointerV1, u32> for Observer {
    fn event(
        state: &mut Self,
        _: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
            ..
        } = event
        {
            state
                .constraints
                .relative
                .push((*id, dx, dy, dx_unaccel, dy_unaccel));
        }
    }
}

fn window(f: &mut Fixture, id: u32) -> (wl_surface::WlSurface, SurfaceId) {
    let surface = f.surface(id);
    let (id, _) = f.toplevel(&surface);
    surface.attach(Some(&f.buffer()), 0, 0);
    surface.commit();
    f.sync();
    (surface, id)
}
fn route(surface: SurfaceId) -> ClientPointerRoute {
    ClientPointerRoute {
        surface,
        layer: SurfaceLayerId::new(1),
        transform: InputTransform::IDENTITY,
    }
}
fn motion(
    f: &mut Fixture,
    route: ClientPointerRoute,
    position: InputPosition,
    relative: Option<RelativeMotion>,
) -> InputPosition {
    let event = f.server.constrain_local_input(
        RawSeatEvent::new(RawSeatEventKind::PointerMotion { position, relative }, 10),
        Some(route),
    );
    let RawSeatEventKind::PointerMotion { position, relative } = event.event else {
        panic!("motion");
    };
    f.server.apply_client_input(ClientInputEvent {
        target: ClientInputTarget::Pointer {
            surface: route.surface,
            layer: route.layer,
        },
        host_position: Some(position),
        event: InputEventKind::PointerMotion {
            position: route.transform.transform(position),
            relative,
        },
        time: event.time,
    });
    f.sync();
    position
}
fn pointer(f: &Fixture) -> (u32, wl_pointer::WlPointer) {
    let (&id, pointer) = f
        .observer
        .constraints
        .pointers
        .iter()
        .min_by_key(|(id, _)| *id)
        .expect("desktop pointer");
    (id, pointer.clone())
}
fn lock(
    f: &Fixture,
    surface: &wl_surface::WlSurface,
    pointer: &wl_pointer::WlPointer,
    lifetime: Lifetime,
    id: u32,
) -> ZwpLockedPointerV1 {
    f.observer
        .constraints
        .manager
        .as_ref()
        .expect("constraints global")
        .lock_pointer(surface, pointer, None, lifetime, &f.queue.handle(), id)
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn pointer_lock_preserves_relative_motion_hiding_and_unlock_hint() {
    let mut f = Fixture::new();
    let (surface, window) = window(&mut f, 1801);
    let route = route(window);
    f.server.route_controller_focus(None, Some(window));
    motion(&mut f, route, InputPosition::new(0.5, 0.5), None);
    let (seat, pointer) = pointer(&f);
    f.observer
        .constraints
        .relative_manager
        .as_ref()
        .expect("relative global")
        .get_relative_pointer(&pointer, &f.queue.handle(), seat);
    let lock = lock(&f, &surface, &pointer, Lifetime::Persistent, 1);
    pointer.set_cursor(f.observer.constraints.enters[&seat], None, 0, 0);
    f.sync();
    assert_eq!(f.observer.constraints.locks, [(1, true)]);
    let input = f
        .server
        .local_input
        .pointer_binding
        .borrow()
        .clone()
        .expect("input binding");
    assert_eq!(*input.cursor_status.borrow(), CursorImageStatus::Hidden);
    let absolute_before = f.observer.constraints.motions.len();
    let relative = RelativeMotion {
        delta: InputDelta::new(100., -50.),
        unaccelerated: InputDelta::new(80., -40.),
        time_micros: 10_000,
    };
    for _ in 0..3 {
        assert_eq!(
            motion(
                &mut f,
                route,
                InputPosition::new(100.5, -49.5),
                Some(relative)
            ),
            InputPosition::new(0.5, 0.5)
        );
    }
    for state in [
        weld_client::ButtonState::Pressed,
        weld_client::ButtonState::Released,
    ] {
        f.server.apply_client_input(ClientInputEvent {
            target: ClientInputTarget::Pointer {
                surface: window,
                layer: route.layer,
            },
            host_position: None,
            event: InputEventKind::PointerButton {
                position: None,
                button: weld_client::LinuxButtonCode(272),
                state,
            },
            time: 11,
        });
    }
    f.sync();
    assert_eq!(
        f.observer.input.buttons.len(),
        2,
        "buttons survive pointer locking"
    );
    assert_eq!(
        f.observer.constraints.motions.len(),
        absolute_before,
        "locked clients receive no absolute motion"
    );
    assert_eq!(
        f.observer.constraints.relative,
        vec![(seat, 100., -50., 80., -40.); 3]
    );
    assert_eq!(
        *input.cursor_status.borrow(),
        CursorImageStatus::Hidden,
        "lock never resets the game's hidden cursor"
    );
    lock.set_cursor_position_hint(1.5, 1.5);
    surface.commit();
    f.sync();
    lock.destroy();
    f.sync();
    let warp = f
        .server
        .take_pointer_warp(Some(route))
        .expect("committed unlock hint");
    assert!(
        matches!(warp.event, RawSeatEventKind::PointerMotion { position, relative: None } if position == InputPosition::new(1.5, 1.5))
    );
    assert_eq!(f.server.local_pointer_constraint(), None);
    let next_lock = self::lock(&f, &surface, &pointer, Lifetime::Persistent, 2);
    next_lock.set_cursor_position_hint(1.25, 1.25);
    surface.commit();
    f.sync();
    next_lock.destroy();
    f.sync();
    f.server.route_controller_focus(None, None);
    assert!(
        f.server.take_pointer_warp(Some(route)).is_none(),
        "focus loss between hint submission and consumption cancels the warp"
    );
    f.server.route_controller_input(
        None,
        ClientInputEvent {
            target: ClientInputTarget::Pointer {
                surface: window,
                layer: route.layer,
            },
            host_position: Some(InputPosition::new(3., 3.)),
            event: InputEventKind::PointerLeft {
                position: InputPosition::new(3., 3.),
            },
            time: 20,
        },
    );
    f.sync();
    assert!(
        f.server.local_input.shell_owns_cursor.get(),
        "desktop regains cursor ownership"
    );
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn pointer_lock_tracks_keyboard_focus_visibility_and_oneshot_lifetime() {
    let mut f = Fixture::new();
    let (surface, first) = window(&mut f, 1811);
    let (_, second) = window(&mut f, 1812);
    let route = route(first);
    f.server.route_controller_focus(None, Some(second));
    motion(&mut f, route, InputPosition::new(0.5, 0.5), None);
    let (_, pointer) = pointer(&f);
    let persistent = lock(&f, &surface, &pointer, Lifetime::Persistent, 1);
    f.sync();
    assert!(
        f.observer.constraints.locks.is_empty(),
        "hover alone cannot lock an unfocused window"
    );
    f.server.route_controller_focus(None, Some(first));
    f.sync();
    assert_eq!(f.observer.constraints.locks, [(1, true)]);
    f.server.route_controller_focus(None, Some(second));
    f.sync();
    assert_eq!(f.observer.constraints.locks.last(), Some(&(1, false)));
    f.server.route_controller_focus(None, Some(first));
    f.server.set_local_presentation(first, None);
    f.sync();
    assert_eq!(
        f.server.local_pointer_constraint(),
        None,
        "workspace-hidden window releases capture"
    );
    f.server
        .set_local_presentation(first, Some(PresentationRate::HZ_60));
    f.sync();
    assert_eq!(
        f.server.local_pointer_constraint(),
        Some(crate::server::PointerConstraintMode::Locked)
    );
    persistent.destroy();
    f.sync();
    let _oneshot = lock(&f, &surface, &pointer, Lifetime::Oneshot, 2);
    f.sync();
    f.server.route_controller_focus(None, Some(second));
    f.sync();
    f.server.route_controller_focus(None, Some(first));
    f.sync();
    assert_eq!(
        f.observer
            .constraints
            .locks
            .iter()
            .filter(|(id, _)| *id == 2)
            .copied()
            .collect::<Vec<_>>(),
        [(2, true), (2, false)]
    );
    assert_eq!(f.server.local_pointer_constraint(), None);
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn pointer_confinement_uses_committed_regions_and_scaled_coordinates() {
    let mut f = Fixture::new();
    let (surface, window) = window(&mut f, 1821);
    let mut route = route(window);
    route.transform = InputTransform {
        xx: 2.,
        yy: 2.,
        x: -100.,
        y: -200.,
        ..InputTransform::IDENTITY
    };
    f.server.route_controller_focus(None, Some(window));
    motion(&mut f, route, InputPosition::new(50.25, 100.25), None);
    let (_, pointer) = pointer(&f);
    let region = f
        .observer
        .compositor
        .as_ref()
        .expect("compositor")
        .create_region(&f.queue.handle(), ());
    region.add(0, 0, 1, 2);
    let confined = f
        .observer
        .constraints
        .manager
        .as_ref()
        .expect("constraints")
        .confine_pointer(
            &surface,
            &pointer,
            Some(&region),
            Lifetime::Persistent,
            &f.queue.handle(),
            1,
        );
    f.sync();
    assert_eq!(f.observer.constraints.confinements, [(1, true)]);
    let clipped = motion(&mut f, route, InputPosition::new(80., 100.75), None);
    assert!(
        (50.25..50.5).contains(&clipped.x),
        "confined position {clipped:?}"
    );
    assert!(
        (clipped.y - 100.75).abs() < 0.0001,
        "motion slides along the edge"
    );
    let outside = f
        .observer
        .compositor
        .as_ref()
        .expect("compositor")
        .create_region(&f.queue.handle(), ());
    outside.add(1, 0, 1, 2);
    confined.set_region(Some(&outside));
    f.sync();
    assert_eq!(
        f.observer.constraints.confinements,
        [(1, true)],
        "pending region is not applied early"
    );
    surface.commit();
    f.sync();
    assert_eq!(
        f.observer.constraints.confinements.last(),
        Some(&(1, false)),
        "committed region excludes the current pointer"
    );
    motion(&mut f, route, InputPosition::new(50.75, 100.5), None);
    assert_eq!(f.observer.constraints.confinements.last(), Some(&(1, true)));
    surface.attach(None, 0, 0);
    surface.commit();
    f.sync();
    assert_eq!(
        f.server.local_pointer_constraint(),
        None,
        "unmapping retires confinement"
    );
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn pointer_lock_host_focus_loss_releases_and_does_not_reactivate_without_focus() {
    let mut f = Fixture::new();
    let (surface, window) = window(&mut f, 1831);
    let route = route(window);
    f.server.route_controller_focus(None, Some(window));
    motion(&mut f, route, InputPosition::new(0.5, 0.5), None);
    let (_, pointer) = pointer(&f);
    let _lock = lock(&f, &surface, &pointer, Lifetime::Persistent, 1);
    f.sync();
    f.server.release_host_input(20);
    f.sync();
    assert_eq!(f.observer.constraints.locks, [(1, true), (1, false)]);
    assert_eq!(f.server.local_pointer_constraint(), None);
    motion(&mut f, route, InputPosition::new(0.5, 0.5), None);
    assert_eq!(f.observer.constraints.locks.len(), 2);
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn pointer_lock_remote_seat_cannot_capture_the_desktop() {
    let mut f = Fixture::new();
    let (surface, window) = window(&mut f, 1841);
    let controller = weld_client::ClientInputController {
        adapter: ClientSourceId::new(9),
        connection: 42,
    };
    f.server.remote_input(controller).expect("remote seat");
    f.server.advertise_remote_input(controller, &f.client);
    f.sync();
    f.sync();
    f.server
        .route_controller_focus(Some(controller), Some(window));
    f.server.route_controller_input(
        Some(controller),
        ClientInputEvent {
            target: ClientInputTarget::Pointer {
                surface: window,
                layer: SurfaceLayerId::new(1),
            },
            host_position: None,
            event: InputEventKind::PointerMotion {
                position: InputPosition::new(0.5, 0.5),
                relative: None,
            },
            time: 1,
        },
    );
    f.server.set_local_presentation(window, None);
    f.sync();
    let input = f.server.remote_inputs[&controller]
        .pointer_binding
        .borrow()
        .clone()
        .expect("remote binding");
    let native = input
        .native
        .get_pointer()
        .expect("native pointer")
        .client_pointers(&f.client)
        .next()
        .expect("bound pointer");
    let pointer = f
        .observer
        .constraints
        .pointers
        .values()
        .find(|pointer| {
            pointer.id().protocol_id()
                == smithay::reexports::wayland_server::Resource::id(&native).protocol_id()
        })
        .expect("remote protocol pointer")
        .clone();
    let _lock = lock(&f, &surface, &pointer, Lifetime::Persistent, 1);
    f.sync();
    assert_eq!(
        f.observer.constraints.locks,
        [(1, true)],
        "remote input remains independent of local workspace visibility"
    );
    assert_eq!(f.server.local_pointer_constraint(), None);
    let event = f.server.constrain_local_input(
        RawSeatEvent::new(
            RawSeatEventKind::PointerMotion {
                position: InputPosition::new(500., 200.),
                relative: None,
            },
            2,
        ),
        Some(route(window)),
    );
    assert!(
        matches!(event.event, RawSeatEventKind::PointerMotion { position, .. } if position == InputPosition::new(500., 200.))
    );
    f.server.retire_remote_seat(controller);
    f.sync();
    assert_eq!(f.observer.constraints.locks.last(), Some(&(1, false)));
}

#[path = "pointer_constraints_x11_tests.rs"]
mod x11;
