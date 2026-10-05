use super::*;
use crate::{MAX_TOUCH_CONTACTS, TouchEvent, TouchId};

#[test]
fn contacts_capture_distinct_routes_and_transform_without_moving_pointer_hover() {
    let mut runtime = ClientRuntime::default();
    let record = register(&mut runtime, 1);
    let a = ClientPointerRoute {
        surface: surface(1, 1, 1),
        layer: SurfaceLayerId::new(1),
        transform: InputTransform {
            xx: 2.0,
            yy: 3.0,
            x: -10.0,
            ..InputTransform::IDENTITY
        },
    };
    let b = ClientPointerRoute {
        surface: surface(1, 1, 2),
        transform: InputTransform::IDENTITY,
        ..a
    };
    for (id, route) in [(TouchId(1), a), (TouchId(2), b)] {
        assert!(
            runtime
                .dispatch_touch(
                    Some(route),
                    TouchEvent::Down {
                        id,
                        position: InputPosition::new(10.0, 10.0)
                    },
                    1
                )
                .is_delivered()
        );
    }
    runtime.dispatch_touch(
        Some(b),
        TouchEvent::Motion {
            id: TouchId(1),
            position: InputPosition::new(20.0, 30.0),
        },
        2,
    );
    runtime.dispatch_touch(None, TouchEvent::Frame, 2);
    assert!(runtime.pointer_route.is_none());
    let record = record.borrow();
    assert_eq!(record.inputs[2].target.surface(), a.surface);
    assert_eq!(
        record.inputs[2].event,
        InputEventKind::Touch {
            event: TouchEvent::Motion {
                id: TouchId(1),
                position: InputPosition::new(30.0, 90.0)
            }
        }
    );
    assert_eq!(
        record
            .inputs
            .iter()
            .filter(|event| event.event
                == InputEventKind::Touch {
                    event: TouchEvent::Frame
                })
            .count(),
        2
    );
}

#[test]
fn focus_loss_and_surface_retirement_cancel_contacts_without_orphaned_tails() {
    let mut runtime = ClientRuntime::default();
    let record = register(&mut runtime, 1);
    let route = ClientPointerRoute {
        surface: surface(1, 1, 1),
        layer: SurfaceLayerId::new(1),
        transform: InputTransform::IDENTITY,
    };
    let down = TouchEvent::Down {
        id: TouchId(1),
        position: InputPosition::default(),
    };
    runtime.dispatch_touch(Some(route), down, 1);
    runtime.dispatch_touch(None, TouchEvent::Frame, 1);
    runtime.host_focus_lost(2);
    assert!(
        !runtime
            .dispatch_touch(Some(route), TouchEvent::Up { id: TouchId(1) }, 3)
            .is_delivered()
    );
    runtime.dispatch_touch(Some(route), down, 4);
    runtime.forget_surface(route.surface);
    assert!(
        !runtime
            .dispatch_touch(Some(route), TouchEvent::Up { id: TouchId(1) }, 5)
            .is_delivered()
    );
    assert_eq!(
        record
            .borrow()
            .inputs
            .iter()
            .filter(|input| input.event
                == InputEventKind::Touch {
                    event: TouchEvent::Cancel
                })
            .count(),
        2
    );
}

#[test]
fn retiring_a_surface_cancels_each_captured_layer() {
    let mut runtime = ClientRuntime::default();
    let record = register(&mut runtime, 1);
    let surface = surface(1, 1, 1);
    for layer in [1, 2] {
        runtime.dispatch_touch(
            Some(ClientPointerRoute {
                surface,
                layer: SurfaceLayerId::new(layer),
                transform: InputTransform::IDENTITY,
            }),
            TouchEvent::Down {
                id: TouchId(layer),
                position: InputPosition::default(),
            },
            1,
        );
    }
    runtime.dispatch_touch(None, TouchEvent::Frame, 1);
    runtime.forget_surface(surface);
    let layers = record
        .borrow()
        .inputs
        .iter()
        .filter_map(|input| match (input.target, &input.event) {
            (
                ClientInputTarget::Touch { layer, .. },
                InputEventKind::Touch {
                    event: TouchEvent::Cancel,
                },
            ) => Some(layer),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(layers, [SurfaceLayerId::new(1), SurfaceLayerId::new(2)]);
}

#[test]
fn invalid_duplicate_and_excess_contacts_do_not_enter_the_capture_ledger() {
    let mut runtime = ClientRuntime::default();
    register(&mut runtime, 1);
    let route = ClientPointerRoute {
        surface: surface(1, 1, 1),
        layer: SurfaceLayerId::new(1),
        transform: InputTransform::IDENTITY,
    };
    assert!(
        !runtime
            .dispatch_touch(
                Some(route),
                TouchEvent::Down {
                    id: TouchId(0),
                    position: InputPosition::new(f64::NAN, 0.0)
                },
                0
            )
            .is_delivered()
    );
    for index in 0..MAX_TOUCH_CONTACTS {
        assert!(
            runtime
                .dispatch_touch(
                    Some(route),
                    TouchEvent::Down {
                        id: TouchId(index as u64),
                        position: InputPosition::default()
                    },
                    1
                )
                .is_delivered()
        );
    }
    for id in [TouchId(0), TouchId(u64::MAX)] {
        assert!(
            !runtime
                .dispatch_touch(
                    Some(route),
                    TouchEvent::Down {
                        id,
                        position: InputPosition::default()
                    },
                    1
                )
                .is_delivered()
        );
    }
}
