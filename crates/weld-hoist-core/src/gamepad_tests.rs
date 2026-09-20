use super::*;
use std::{cell::RefCell, rc::Rc};

#[derive(Default)]
pub(crate) struct DeviceLog {
    pub opened: usize,
    pub dropped: usize,
    pub states: Vec<GamepadState>,
}
pub(crate) struct Provider(pub Rc<RefCell<DeviceLog>>);
struct Device(Rc<RefCell<DeviceLog>>);
impl GamepadProvider for Provider {
    fn open(&mut self) -> HoistPortResult<Box<dyn GamepadDevice>> {
        self.0.borrow_mut().opened += 1;
        Ok(Box::new(Device(self.0.clone())))
    }
}
impl GamepadDevice for Device {
    fn update(&mut self, state: GamepadState) -> HoistPortResult<()> {
        self.0.borrow_mut().states.push(state);
        Ok(())
    }
}
impl Drop for Device {
    fn drop(&mut self) {
        self.0.borrow_mut().dropped += 1;
    }
}

#[test]
fn timeout_neutralizes_device_and_old_capture_cannot_restart() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let mut source = GamepadSource::new(Some(Box::new(Provider(log.clone()))));
    let now = Instant::now();
    source.accept(GamepadRequest::Begin { generation: 1 }, now);
    let mut held = GamepadState {
        left: [123, -100],
        ..Default::default()
    };
    held.buttons.south = true;
    source.accept(
        GamepadRequest::State {
            generation: 1,
            state: held,
        },
        now,
    );
    assert!(
        source
            .expire(now + WATCHDOG - Duration::from_millis(1))
            .is_none()
    );
    assert_eq!(
        source.expire(now + WATCHDOG),
        Some(GamepadStatus::Capture {
            generation: 1,
            state: GamepadCaptureState::TimedOut
        })
    );
    assert_eq!(log.borrow().states.last(), Some(&GamepadState::default()));
    assert_eq!(log.borrow().dropped, 1);
    source.accept(GamepadRequest::Begin { generation: 1 }, now + WATCHDOG);
    source.accept(
        GamepadRequest::State {
            generation: 1,
            state: held,
        },
        now + WATCHDOG,
    );
    assert!(source.deadline().is_none());
    source.accept(GamepadRequest::Begin { generation: 2 }, now + WATCHDOG);
    assert_eq!(log.borrow().opened, 2);
    drop(source);
    assert_eq!(log.borrow().dropped, 2);
    assert_eq!(log.borrow().states.last(), Some(&GamepadState::default()));
}

#[test]
fn cancelled_begin_and_unconfigured_source_never_create_device() {
    let log = Rc::new(RefCell::new(DeviceLog::default()));
    let mut source = GamepadSource::new(Some(Box::new(Provider(log.clone()))));
    let now = Instant::now();
    source.accept(GamepadRequest::End { generation: 1 }, now);
    source.accept(GamepadRequest::Begin { generation: 1 }, now);
    assert_eq!(log.borrow().opened, 0);
    let mut disabled = GamepadSource::default();
    assert!(disabled.advertise().is_none());
    assert_eq!(
        disabled.accept(GamepadRequest::Begin { generation: 1 }, now),
        Some(GamepadStatus::Capture {
            generation: 1,
            state: GamepadCaptureState::Denied
        })
    );
}

fn active() -> GamepadController {
    let controller = GamepadController::default();
    assert!(controller.begin().is_none());
    controller.observe(GamepadStatus::Available);
    assert_eq!(controller.begin(), Some(1));
    assert_eq!(
        controller.pop(),
        Some(GamepadRequest::Begin { generation: 1 })
    );
    assert!(!controller.sample(1, GamepadState::default(), Instant::now()));
    controller.observe(GamepadStatus::Capture {
        generation: 1,
        state: GamepadCaptureState::Active,
    });
    controller
}

#[test]
fn mailbox_preserves_button_edges_and_cancels_out_of_band_on_overflow() {
    let controller = active();
    let now = Instant::now();
    for axis in 1..100 {
        assert!(controller.sample(
            1,
            GamepadState {
                left: [axis, 0],
                ..Default::default()
            },
            now
        ));
    }
    assert_eq!(controller.lock().queue.len(), 1);
    assert!(
        matches!(controller.pop(), Some(GamepadRequest::State { state, .. }) if state.left[0] == 99)
    );
    for index in 0..CAPACITY {
        let mut state = GamepadState::default();
        state.buttons.south = index % 2 == 0;
        assert!(controller.sample(1, state, now));
    }
    let mut state = GamepadState::default();
    state.buttons.south = true;
    assert!(!controller.sample(1, state, now));
    assert_eq!(controller.mode(), GamepadMode::Ready);
    assert_eq!(
        controller.pop(),
        Some(GamepadRequest::End { generation: 1 })
    );
    assert!(controller.pop().is_none());
    controller.observe(GamepadStatus::Capture {
        generation: 1,
        state: GamepadCaptureState::Active,
    });
    assert_eq!(controller.mode(), GamepadMode::Ready);
}

#[test]
fn reconnect_cannot_reuse_old_handle_and_only_live_samples_renew() {
    let controller = active();
    let now = Instant::now();
    assert!(controller.sample(1, GamepadState::default(), now));
    assert!(controller.pop().is_some());
    assert!(controller.sample(1, GamepadState::default(), now + RENEWAL / 2));
    assert!(controller.pop().is_none());
    assert!(controller.sample(1, GamepadState::default(), now + RENEWAL));
    assert!(controller.pop().is_some());
    controller.disconnect();
    controller.observe(GamepadStatus::Available);
    assert_eq!(controller.mode(), GamepadMode::Disconnected);
    assert!(controller.begin().is_none());
}
