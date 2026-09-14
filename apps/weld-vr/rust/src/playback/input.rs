//! Bounded main-thread input admission; the receiver thread owns ClientRuntime.
//! Reset is out-of-band so a full queue cannot strand a physical hold.
mod keys;

use std::{
    collections::{HashSet, VecDeque},
    sync::OnceLock,
    time::Instant,
};
use weld_client::{
    ButtonState, ClientCursor, ClientFocusRequest, ClientPointerRoute, ClientPointerRouteUpdate,
    ClientRequest, ClientRuntime, InputEventKind, InputPosition, KeyboardKeyState, LinuxButtonCode,
    LinuxKeycode, RawScrollFrame, RawScrollPhase, RawScrollSource, RuntimeInputEvent,
    RuntimeInputEventKind, SurfaceInputGeometry,
};

pub(super) use keys::physical_key;
const CAPACITY: usize = 128;
static INPUT_CLOCK: OnceLock<Instant> = OnceLock::new();

#[derive(Clone, Debug)]
pub(super) struct Target {
    pub epoch: u64,
    pub geometry: SurfaceInputGeometry,
}

#[derive(Clone, Debug)]
enum Action {
    Pointer {
        route: Option<ClientPointerRoute>,
        position: InputPosition,
        event: Option<InputEventKind>,
        focus: bool,
    },
    Key {
        keycode: LinuxKeycode,
        state: KeyboardKeyState,
    },
}

#[derive(Clone, Debug)]
struct Message {
    epoch: u64,
    time: u32,
    action: Action,
}

pub(super) struct InputState {
    pub epoch: u64,
    queue: VecDeque<Message>,
    reset: Option<u32>,
    cached_pointer: Option<(ClientPointerRoute, InputPosition)>,
    keys: HashSet<LinuxKeycode>,
    buttons: HashSet<LinuxButtonCode>,
    suppressed_keys: HashSet<LinuxKeycode>,
    suppressed_buttons: HashSet<LinuxButtonCode>,
    keyboard_focus: bool,
    capture_route: Option<ClientPointerRoute>,
    cursor: ClientCursor,
    cursor_dirty: bool,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            epoch: 0,
            queue: VecDeque::with_capacity(CAPACITY),
            reset: None,
            cached_pointer: None,
            keys: HashSet::new(),
            buttons: HashSet::new(),
            suppressed_keys: HashSet::new(),
            suppressed_buttons: HashSet::new(),
            keyboard_focus: false,
            capture_route: None,
            cursor: ClientCursor::default(),
            cursor_dirty: true,
        }
    }
}

impl InputState {
    fn time(&self) -> u32 {
        // Wayland timestamps wrap modulo 2^32 milliseconds. Queue order is
        // monotonic elapsed time; coalescing and reset always take the newest.
        INPUT_CLOCK.get_or_init(Instant::now).elapsed().as_millis() as u32
    }
    pub fn reset(&mut self) {
        self.queue.clear();
        self.reset = Some(self.time());
        self.suppressed_keys.extend(self.keys.iter().copied());
        self.suppressed_buttons.extend(self.buttons.iter().copied());
        self.keyboard_focus = false;
        self.capture_route = None;
        self.cached_pointer = None;
        self.set_cursor(ClientCursor::default());
    }
    pub fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.reset();
    }
    fn enqueue(&mut self, action: Action) -> bool {
        let pointer = match &action {
            Action::Pointer {
                route, position, ..
            } => route.map(|route| (route, *position)),
            Action::Key { .. } => self.cached_pointer,
        };
        let message = Message {
            epoch: self.epoch,
            time: self.time(),
            action,
        };
        if let Action::Pointer {
            route,
            event: None,
            focus: false,
            ..
        } = &message.action
            && let Some(last) = self.queue.back_mut()
            && last.epoch == message.epoch
            && matches!(&last.action, Action::Pointer { route: old, event: None, focus: false, .. } if old == route)
        {
            *last = message;
            self.cached_pointer = pointer;
            return true;
        }
        if self.queue.len() == CAPACITY {
            self.reset();
            return false;
        }
        self.queue.push_back(message);
        self.cached_pointer = pointer;
        true
    }
    pub fn key(&mut self, keycode: LinuxKeycode, state: KeyboardKeyState) -> bool {
        match state {
            KeyboardKeyState::Pressed => {
                if !self.keys.insert(keycode) {
                    return false;
                }
                if !self.keyboard_focus {
                    self.suppressed_keys.insert(keycode);
                    return false;
                }
            }
            KeyboardKeyState::Released => {
                let held = self.keys.remove(&keycode);
                if self.suppressed_keys.remove(&keycode) || !held {
                    return false;
                }
            }
            KeyboardKeyState::Repeated => {
                if !self.keys.contains(&keycode)
                    || self.suppressed_keys.contains(&keycode)
                    || !self.keyboard_focus
                {
                    return false;
                }
            }
        }
        self.enqueue(Action::Key { keycode, state })
    }
    /// Button indices and wheel directions are Godot's scalar input contract.
    pub fn pointer(
        &mut self,
        target: Option<&Target>,
        rectangle: [f64; 4],
        position: InputPosition,
        button_index: i64,
        pressed: bool,
    ) -> bool {
        if !position.x.is_finite() || !position.y.is_finite() {
            return false;
        }
        let hit = target
            .filter(|target| target.epoch == self.epoch)
            .and_then(|target| target.geometry.pointer_route(rectangle, position));
        let route = hit.or(self.capture_route);
        if button_index == 0 {
            self.enqueue(Action::Pointer {
                route,
                position,
                event: None,
                focus: false,
            });
            return route.is_some() || self.has_admitted_buttons();
        }
        if (4..=7).contains(&button_index) {
            return self.wheel(route, position, button_index, pressed);
        }
        let button = LinuxButtonCode(match button_index {
            1 => 0x110,
            2 => 0x111,
            3 => 0x112,
            8 => 0x113,
            9 => 0x114,
            _ => return false,
        });
        if pressed {
            if !self.buttons.insert(button) {
                return false;
            }
            if route.is_none() {
                self.reset();
                return false;
            }
            self.keyboard_focus = true;
            if self.capture_route.is_none() {
                self.capture_route = route;
            }
        } else {
            let held = self.buttons.remove(&button);
            let suppressed = self.suppressed_buttons.remove(&button);
            if !self.has_admitted_buttons() {
                self.capture_route = None;
            }
            if suppressed || !held {
                return false;
            }
        }
        let route = if !pressed && !self.has_admitted_buttons() {
            hit
        } else {
            route
        };
        self.enqueue(Action::Pointer {
            route,
            position,
            focus: pressed,
            event: Some(InputEventKind::PointerButton {
                position: Some(position),
                button,
                state: if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
            }),
        })
    }
    fn has_admitted_buttons(&self) -> bool {
        self.buttons
            .iter()
            .any(|button| !self.suppressed_buttons.contains(button))
    }
    /// Discrete wheel ticks keep their last admitted hover/capture route while
    /// rendering binds a new frame. Never consult that pending frame's geometry.
    pub fn during_bind(&mut self, button: i64, pressed: bool) -> bool {
        if !(4..=7).contains(&button) {
            return false;
        }
        let Some((route, position)) = self.cached_pointer else {
            return false;
        };
        self.wheel(Some(route), position, button, pressed)
    }
    fn wheel(
        &mut self,
        route: Option<ClientPointerRoute>,
        position: InputPosition,
        button: i64,
        pressed: bool,
    ) -> bool {
        if !pressed || route.is_none() {
            return false;
        }
        let vertical = match button {
            4 => -1,
            5 => 1,
            _ => 0,
        };
        let horizontal = match button {
            6 => -1,
            7 => 1,
            _ => 0,
        };
        self.enqueue(Action::Pointer {
            route,
            position,
            focus: false,
            event: Some(InputEventKind::PointerAxis {
                position: Some(position),
                axis: RawScrollFrame {
                    source: RawScrollSource::Wheel,
                    phase: RawScrollPhase::Moved,
                    horizontal: f64::from(horizontal) * 15.0,
                    vertical: f64::from(vertical) * 15.0,
                    horizontal_v120: (horizontal != 0).then_some(horizontal * 120),
                    vertical_v120: (vertical != 0).then_some(vertical * 120),
                    horizontal_stop: false,
                    vertical_stop: false,
                },
            }),
        })
    }
    pub fn set_cursor(&mut self, cursor: ClientCursor) {
        if self.cursor != cursor {
            self.cursor = cursor;
            self.cursor_dirty = true;
        }
    }
    pub fn take_cursor(&mut self) -> Option<ClientCursor> {
        if !std::mem::take(&mut self.cursor_dirty) {
            return None;
        }
        Some(self.cursor.clone())
    }
}

/// No mailbox lock is held while adapter callbacks run.
pub(super) fn service(shared: &super::Shared, runtime: &mut ClientRuntime) {
    let (reset, messages, epoch) = {
        let mut input = super::lock(&shared.input);
        (
            input.reset.take(),
            input.queue.drain(..).collect::<Vec<_>>(),
            input.epoch,
        )
    };
    if let Some(time) = reset {
        runtime.host_focus_lost(time);
    }
    for message in messages {
        if message.epoch != epoch {
            continue;
        }
        match message.action {
            Action::Pointer {
                route,
                position,
                event,
                focus,
            } => {
                runtime.publish_pointer_route(ClientPointerRouteUpdate {
                    route,
                    position,
                    time: message.time,
                });
                if focus && let Some(route) = route {
                    runtime.apply_request(ClientRequest::Focus(ClientFocusRequest {
                        source: route.surface.source(),
                        surface: Some(route.surface),
                    }));
                }
                if let Some(event) = event {
                    runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
                        RuntimeInputEventKind::Input(event),
                        message.time,
                    ));
                }
            }
            Action::Key { keycode, state } => {
                runtime.dispatch_unconsumed_input(RuntimeInputEvent::new(
                    RuntimeInputEventKind::Input(InputEventKind::Keyboard { keycode, state }),
                    message.time,
                ));
            }
        }
    }
    super::lock(&shared.input).set_cursor(
        runtime
            .pointer_cursor()
            .map_or_else(ClientCursor::default, |(_, cursor)| cursor),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    use weld_client::{
        ClientAdapter, ClientAdapterCommandEnvelope, ClientEventQueue, ClientId, ClientInputEvent,
        ClientProvenance, ClientRuntimeAdapter, ClientSourceDescriptor, ClientSourceId,
        ClientSurfaceId, LogicalPoint, LogicalSize, SurfaceInputPlacement, SurfaceInputRect,
        SurfaceLayerId,
    };

    fn target(epoch: u64) -> Target {
        Target {
            epoch,
            geometry: SurfaceInputGeometry {
                surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1),
                origin: InputPosition::default(),
                logical_size: [100.0, 100.0],
                inputs: vec![SurfaceInputPlacement {
                    layer: SurfaceLayerId::new(1),
                    position: LogicalPoint::ZERO,
                    regions: vec![SurfaceInputRect {
                        position: LogicalPoint::ZERO,
                        size: LogicalSize::new(100.0, 100.0),
                    }],
                }],
            },
        }
    }
    const RECT: [f64; 4] = [0.0, 0.0, 200.0, 200.0];
    const POINT: InputPosition = InputPosition::new(50.0, 50.0);

    #[test]
    fn pending_bind_keeps_discrete_wheels_on_last_admitted_route() {
        let mut state = InputState::default();
        let target = target(0);
        assert!(!state.during_bind(4, true));
        state.pointer(Some(&target), RECT, POINT, 0, false);
        for _ in 0..5 {
            assert!(state.during_bind(4, true));
        }
        assert!(!state.during_bind(4, false));
        assert!(!state.during_bind(1, true));
        assert_eq!(state.queue.len(), 6);
        for message in state.queue.iter().skip(1) {
            assert!(
                matches!(&message.action, Action::Pointer { route: Some(route), position, event: Some(InputEventKind::PointerAxis { .. }), .. }
                if route.surface == target.geometry.surface && *position == POINT)
            );
        }
        state.pointer(
            Some(&target),
            RECT,
            InputPosition::new(300.0, 50.0),
            0,
            false,
        );
        assert!(!state.during_bind(4, true));
        state.pointer(Some(&target), RECT, POINT, 0, false);
        state.invalidate();
        assert!(!state.during_bind(4, true));
    }

    #[test]
    fn suppressed_hold_cannot_leave_phantom_capture_after_another_button_releases() {
        let mut state = InputState::default();
        let target = target(0);
        state.pointer(Some(&target), RECT, POINT, 1, true);
        state.reset();
        state.pointer(Some(&target), RECT, POINT, 2, true);
        state.pointer(Some(&target), RECT, POINT, 2, false);
        let outside = InputPosition::new(300.0, 50.0);
        assert!(!state.pointer(Some(&target), RECT, outside, 0, false));
        assert!(matches!(
            state.queue.back().expect("leave").action,
            Action::Pointer { route: None, .. }
        ));
        assert!(!state.pointer(Some(&target), RECT, outside, 1, false));
        assert!(!state.pointer(Some(&target), RECT, outside, 0, false));
        assert!(state.capture_route.is_none());
    }

    #[test]
    fn new_input_sessions_keep_the_process_timestamp_base() {
        let first = InputState::default();
        let _ = first.time();
        std::thread::sleep(std::time::Duration::from_millis(3));
        let before = first.time();
        let second = InputState::default();
        assert!(second.time() >= before);
    }

    #[test]
    fn adjacent_motion_coalesces_but_buttons_keys_and_wheels_do_not() {
        let mut state = InputState::default();
        let target = target(state.epoch);
        for _ in 0..1000 {
            state.pointer(Some(&target), RECT, POINT, 0, false);
        }
        assert_eq!(state.queue.len(), 1);
        assert!(state.pointer(Some(&target), RECT, POINT, 1, true));
        for _ in 0..10 {
            state.pointer(Some(&target), RECT, POINT, 0, false);
        }
        assert!(state.key(LinuxKeycode(30), KeyboardKeyState::Pressed));
        assert!(state.key(LinuxKeycode(30), KeyboardKeyState::Repeated));
        assert!(state.key(LinuxKeycode(30), KeyboardKeyState::Released));
        assert!(state.pointer(Some(&target), RECT, POINT, 4, true));
        assert!(!state.pointer(Some(&target), RECT, POINT, 4, false));
        assert_eq!(state.queue.len(), 7);
        assert!(matches!(&state.queue.back().expect("wheel").action,
            Action::Pointer { event: Some(InputEventKind::PointerAxis { axis, .. }), .. }
                if axis.vertical == -15.0 && axis.vertical_v120 == Some(-120)));
    }

    #[test]
    fn overflow_reset_cannot_drop_and_holds_need_release_before_reuse() {
        let mut state = InputState::default();
        let target = target(state.epoch);
        state.pointer(Some(&target), RECT, POINT, 1, true);
        state.key(LinuxKeycode(30), KeyboardKeyState::Pressed);
        for _ in 0..CAPACITY {
            state.key(LinuxKeycode(30), KeyboardKeyState::Repeated);
        }
        assert!(state.reset.is_some());
        assert!(state.queue.is_empty());
        // XR retries unadmitted presses, but overflow deliberately suppresses
        // the held button until release. Retrying must not resurrect it.
        for _ in 0..100 {
            assert!(!state.pointer(Some(&target), RECT, POINT, 1, true));
        }
        assert!(state.queue.is_empty());
        assert!(!state.key(LinuxKeycode(30), KeyboardKeyState::Repeated));
        assert!(!state.key(LinuxKeycode(30), KeyboardKeyState::Released));
        assert!(!state.pointer(Some(&target), RECT, POINT, 1, false));
        assert!(state.pointer(Some(&target), RECT, POINT, 1, true));
        assert!(state.key(LinuxKeycode(30), KeyboardKeyState::Pressed));
        assert!(
            state
                .queue
                .iter()
                .all(|event| event.time >= state.reset.expect("reset"))
        );
    }

    #[test]
    fn invalidation_rejects_old_presentation_and_repeat_cannot_create_a_hold() {
        let mut state = InputState::default();
        let old = target(state.epoch);
        state.pointer(Some(&old), RECT, POINT, 1, true);
        state.key(LinuxKeycode(30), KeyboardKeyState::Pressed);
        state.invalidate();
        assert!(!state.key(LinuxKeycode(30), KeyboardKeyState::Repeated));
        assert!(!state.pointer(Some(&old), RECT, POINT, 2, true));
        assert!(state.queue.is_empty());
        assert!(!state.key(LinuxKeycode(31), KeyboardKeyState::Repeated));
    }

    #[derive(Default)]
    struct Observed {
        events: Vec<ClientInputEvent>,
        requests: Vec<ClientRequest>,
        resets: Vec<u32>,
    }
    struct Adapter(Rc<RefCell<Observed>>);
    impl ClientAdapter for Adapter {
        fn drain_events(&mut self, _: &mut ClientEventQueue) {}
        fn apply_request(&mut self, request: ClientRequest) {
            self.0.borrow_mut().requests.push(request);
        }
        fn apply_input(&mut self, event: ClientInputEvent) {
            self.0.borrow_mut().events.push(event);
        }
        fn apply_command(&mut self, _: ClientAdapterCommandEnvelope) {}
        fn host_focus_lost(&mut self, time: u32) {
            self.0.borrow_mut().resets.push(time);
        }
    }

    #[test]
    fn shared_runtime_routes_focus_drag_outside_release_and_repeat_in_order() {
        let shared = super::super::Shared::default();
        let observed = Rc::new(RefCell::new(Observed::default()));
        let mut runtime = ClientRuntime::default();
        runtime
            .register(ClientRuntimeAdapter::new(
                ClientSourceDescriptor::new(ClientSourceId::new(1), ClientProvenance::Local),
                Adapter(observed.clone()),
            ))
            .expect("register");
        let target = target(0);
        {
            let mut input = super::super::lock(&shared.input);
            input.pointer(Some(&target), RECT, POINT, 1, true);
            input.key(LinuxKeycode(30), KeyboardKeyState::Pressed);
            input.key(LinuxKeycode(30), KeyboardKeyState::Repeated);
            input.pointer(
                Some(&target),
                RECT,
                InputPosition::new(300.0, 50.0),
                0,
                false,
            );
            input.pointer(
                Some(&target),
                RECT,
                InputPosition::new(300.0, 50.0),
                1,
                false,
            );
            input.key(LinuxKeycode(30), KeyboardKeyState::Released);
        }
        service(&shared, &mut runtime);
        let record = observed.borrow();
        assert!(matches!(
            record.requests.first(),
            Some(ClientRequest::Focus(_))
        ));
        let keys: Vec<_> = record
            .events
            .iter()
            .filter_map(|event| match event.event {
                InputEventKind::Keyboard { state, .. } => Some(state),
                _ => None,
            })
            .collect();
        assert_eq!(
            keys,
            [
                KeyboardKeyState::Pressed,
                KeyboardKeyState::Repeated,
                KeyboardKeyState::Released
            ]
        );
        assert!(record.events.iter().any(|event| matches!(event.event, InputEventKind::PointerButton { state: ButtonState::Released, position: Some(position), .. } if position.x == 150.0)));
        drop(record);
        super::super::lock(&shared.input).reset();
        service(&shared, &mut runtime);
        assert_eq!(observed.borrow().resets.len(), 1);
        assert!(runtime.pointer_cursor().is_none());
    }
}
