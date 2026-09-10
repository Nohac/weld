use super::*;
use weld_client::{
    ClientFocusRequest, ClientId, ClientInputEvent, ClientSourceId, LinuxKeycode, LogicalPoint,
    PopupState,
};

pub(crate) fn surface(local: u64) -> ClientSurfaceId {
    ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), local)
}
pub(crate) fn session() -> HoistSessionId {
    HoistSessionId::new(1)
}
pub(crate) fn mapped() -> Activity {
    let mut activity = Activity::default();
    for id in [1, 2, 3] {
        activity.register(session(), surface(id));
        activity.mapped(surface(id), true);
    }
    activity
}
pub(crate) fn focus(activity: &mut Activity, local: u64, now: Instant) {
    activity.observe(
        session(),
        &DestinationMessage::Request(ClientRequest::Focus(ClientFocusRequest {
            source: ClientSourceId::new(1),
            surface: Some(surface(local)),
        })),
        now,
        SchedulingPolicy::default(),
    );
}
pub(crate) fn input(activity: &mut Activity, local: u64, event: InputEventKind, now: Instant) {
    let target = if matches!(event, InputEventKind::Keyboard { .. }) {
        ClientInputTarget::Keyboard {
            surface: surface(local),
        }
    } else {
        ClientInputTarget::Pointer {
            surface: surface(local),
            layer: SurfaceLayerId::new(1),
        }
    };
    activity.observe(
        session(),
        &DestinationMessage::input(ClientInputEvent {
            target,
            host_position: None,
            event,
            time: u32::MAX,
        }),
        now,
        SchedulingPolicy::default(),
    );
}
pub(crate) fn press(activity: &mut Activity, local: u64, now: Instant) {
    input(
        activity,
        local,
        InputEventKind::Keyboard {
            keycode: LinuxKeycode(30),
            state: KeyboardKeyState::Pressed,
        },
        now,
    );
}
fn priority(activity: &Activity, local: u64, now: Instant) -> Priority {
    let mut snapshot = ActivitySnapshot::default();
    activity.snapshot(now, SchedulingPolicy::default(), &mut snapshot);
    snapshot.priorities[&activity.group(surface(local)).expect("registered")]
}
fn motion(activity: &mut Activity, local: u64, x: f64, now: Instant) {
    input(
        activity,
        local,
        InputEventKind::PointerMotion {
            position: InputPosition::new(x, 0.0),
        },
        now,
    );
}

#[test]
fn focus_is_modest_and_input_decays_without_a_timer() {
    let mut activity = mapped();
    let now = Instant::now();
    focus(&mut activity, 1, now);
    assert_eq!(priority(&activity, 1, now), Priority::Focused);
    focus(&mut activity, 1, now); // No promotion from duplicate focus.
    press(&mut activity, 1, now);
    assert_eq!(priority(&activity, 1, now), Priority::Interactive);
    assert_eq!(
        priority(&activity, 1, now + Duration::from_secs(1)),
        Priority::Focused
    );
    activity.clear_focus();
    assert_eq!(
        priority(&activity, 1, now + Duration::from_secs(1)),
        Priority::Background
    );
}

#[test]
fn motion_requires_real_changes_and_dwell_not_event_count() {
    let now = Instant::now();
    for interval in [1, 15] {
        let mut activity = mapped();
        focus(&mut activity, 1, now);
        motion(&mut activity, 1, 0.0, now);
        for _ in 0..1000 {
            motion(&mut activity, 1, 0.0, now);
        }
        assert_eq!(priority(&activity, 1, now), Priority::Focused);
        for ms in (15..=90).step_by(interval) {
            motion(&mut activity, 1, ms as f64, now + Duration::from_millis(ms));
        }
        assert_eq!(
            priority(&activity, 1, now + Duration::from_millis(90)),
            Priority::Interactive
        );
        assert_eq!(
            priority(&activity, 1, now + Duration::from_secs(1)),
            Priority::Focused
        );
        motion(&mut activity, 2, 0.0, now);
        motion(&mut activity, 2, 1.0, now);
        assert_eq!(priority(&activity, 2, now), Priority::Background);
    }
}

#[test]
fn release_cleanup_and_remap_never_create_activity() {
    let mut activity = mapped();
    let now = Instant::now();
    focus(&mut activity, 1, now);
    press(&mut activity, 1, now);
    activity.clear_focus();
    let later = now + Duration::from_secs(1);
    input(
        &mut activity,
        1,
        InputEventKind::Keyboard {
            keycode: LinuxKeycode(30),
            state: KeyboardKeyState::Repeated,
        },
        later,
    );
    assert_eq!(priority(&activity, 1, later), Priority::Background);
    input(
        &mut activity,
        1,
        InputEventKind::PointerButton {
            position: Some(InputPosition::new(0.0, 0.0)),
            button: LinuxButtonCode(272),
            state: ButtonState::Pressed,
        },
        later,
    );
    activity.mapped(surface(1), false);
    activity.mapped(surface(1), true);
    motion(&mut activity, 1, 1.0, later);
    motion(&mut activity, 1, 2.0, later);
    input(
        &mut activity,
        1,
        InputEventKind::PointerButton {
            position: None,
            button: LinuxButtonCode(272),
            state: ButtonState::Released,
        },
        later,
    );
    assert_eq!(priority(&activity, 1, later), Priority::Background);
}

#[test]
fn popups_share_attention_but_dialogs_and_sessions_do_not() {
    let mut activity = mapped();
    let now = Instant::now();
    activity.role(
        surface(2),
        ClientSurfaceRole::Popup(PopupState {
            owner: surface(1),
            position: LogicalPoint::new(0.0, 0.0),
            stack_index: 0,
        }),
    );
    focus(&mut activity, 1, now);
    press(&mut activity, 2, now);
    assert_eq!(priority(&activity, 1, now), Priority::Interactive);
    assert_eq!(priority(&activity, 3, now), Priority::Background);
    activity.remove(surface(2));
    assert_eq!(priority(&activity, 1, now), Priority::Focused);
    activity.register(HoistSessionId::new(2), surface(2));
    activity.mapped(surface(2), true);
    activity.role(
        surface(2),
        ClientSurfaceRole::Popup(PopupState {
            owner: surface(1),
            position: LogicalPoint::new(0.0, 0.0),
            stack_index: 0,
        }),
    );
    assert_ne!(activity.group(surface(1)), activity.group(surface(2)));
}

#[test]
fn missing_and_cyclic_popup_owners_are_bounded_and_do_not_merge_groups() {
    let mut activity = mapped();
    for (child, owner) in [(1, 2), (2, 1), (3, 99)] {
        activity.role(
            surface(child),
            ClientSurfaceRole::Popup(PopupState {
                owner: surface(owner),
                position: LogicalPoint::new(0.0, 0.0),
                stack_index: 0,
            }),
        );
    }
    for id in 1..=3 {
        assert_eq!(
            activity.group(surface(id)).expect("fallback").root,
            surface(id)
        );
    }
}

#[test]
fn pointer_layer_handoff_and_cancel_are_not_activity() {
    let mut activity = mapped();
    let now = Instant::now();
    focus(&mut activity, 1, now);
    motion(&mut activity, 1, 10.0, now);
    let event = ClientInputEvent {
        target: ClientInputTarget::Pointer {
            surface: surface(1),
            layer: SurfaceLayerId::new(2),
        },
        host_position: None,
        event: InputEventKind::PointerMotion {
            position: InputPosition::new(99.0, 0.0),
        },
        time: 0,
    };
    activity.observe(
        session(),
        &DestinationMessage::input(event),
        now,
        SchedulingPolicy::default(),
    );
    input(
        &mut activity,
        1,
        InputEventKind::PointerAxis {
            position: None,
            axis: weld_client::RawScrollFrame::cancelled_finger(true, true),
        },
        now,
    );
    assert_eq!(priority(&activity, 1, now), Priority::Focused);
}

#[test]
fn unrelated_keyboard_focus_change_preserves_pointer_drag_attention() {
    let mut activity = mapped();
    let now = Instant::now();
    focus(&mut activity, 1, now);
    input(
        &mut activity,
        2,
        InputEventKind::PointerButton {
            position: Some(InputPosition::new(0.0, 0.0)),
            button: LinuxButtonCode(272),
            state: ButtonState::Pressed,
        },
        now,
    );
    let later = now + Duration::from_secs(1);
    focus(&mut activity, 3, later);
    motion(&mut activity, 2, 1.0, later);
    assert_eq!(priority(&activity, 2, later), Priority::Interactive);
}
