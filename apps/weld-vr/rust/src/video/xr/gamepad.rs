//! XR controller-mode policy. One pointer owns both hands; no window focus or
//! per-pane flag can redirect an active host gamepad.
use std::time::{Duration, Instant};
use weld_hoist_core::gamepad::{GamepadButtons, GamepadController, GamepadMode, GamepadState};

#[derive(Clone, Copy, Default)]
pub(super) struct Hand {
    pub stick: [f32; 2],
    pub trigger: f32,
    pub grip: f32,
    pub a: bool,
    pub b: bool,
    pub click: bool,
}
impl Hand {
    fn valid(self) -> bool {
        self.stick
            .into_iter()
            .chain([self.trigger, self.grip])
            .all(f32::is_finite)
    }
    fn neutral(self) -> bool {
        !self.a
            && !self.b
            && !self.click
            && self.trigger < 0.15
            && self.grip < 0.35
            && self.stick.into_iter().all(|axis| axis.abs() < 0.15)
    }
}
#[derive(Clone, Copy, Default)]
pub(super) struct Hands {
    pub left: Hand,
    pub right: Hand,
}
impl Hands {
    pub fn valid(self) -> bool {
        self.left.valid() && self.right.valid()
    }
    fn neutral(self) -> bool {
        self.left.neutral() && self.right.neutral()
    }
    fn state(self) -> GamepadState {
        GamepadState {
            buttons: GamepadButtons {
                south: self.right.a,
                east: self.right.b,
                west: self.left.a,
                north: self.left.b,
                left_shoulder: self.left.trigger >= 0.5,
                right_shoulder: self.right.trigger >= 0.5,
                select: self.left.click,
                start: self.right.click,
                ..Default::default()
            },
            left: stick(self.left.stick),
            right: stick(self.right.stick),
            triggers: [trigger(self.left.trigger), trigger(self.right.trigger)],
            ..Default::default()
        }
    }
}
fn trigger(value: f32) -> u16 {
    (value.clamp(0.0, 1.0) * f32::from(u16::MAX)).round() as u16
}
fn stick(value: [f32; 2]) -> [i16; 2] {
    // Radial deadzone retains diagonal direction and the full outer range.
    let length = value[0].hypot(value[1]);
    if length <= 0.15 {
        return [0; 2];
    }
    let scale = ((length - 0.15) / 0.85).min(1.0) / length;
    [
        (value[0] * scale * 32767.0).round() as i16,
        (-value[1] * scale * 32767.0).round() as i16,
    ]
}

struct Capture {
    controller: GamepadController,
    generation: u64,
    armed: bool,
    exit_started: Option<Instant>,
}
#[derive(Default)]
pub(super) struct Mode {
    capture: Option<Capture>,
    rearm: bool,
}
impl Mode {
    pub fn enter(&mut self, controller: GamepadController) -> bool {
        if self.capture.is_some() || self.rearm {
            return false;
        }
        let Some(generation) = controller.begin() else {
            return false;
        };
        self.capture = Some(Capture {
            controller,
            generation,
            armed: false,
            exit_started: None,
        });
        true
    }
    pub fn stop(&mut self) {
        if let Some(capture) = self.capture.take() {
            capture.controller.stop();
            self.rearm = true;
        }
    }
    pub fn active(&self) -> bool {
        self.capture.is_some()
    }
    /// True suppresses shell gestures/bindings, including the right-hand neutral
    /// frame that re-arms them after gameplay. While active, the caller may
    /// independently route the right-hand ray and grip to application mouse input.
    pub fn step(&mut self, hands: Option<Hands>, now: Instant) -> bool {
        let hands = hands.filter(|hands| hands.valid());
        let Some(capture) = self.capture.as_mut() else {
            let consumed = self.rearm;
            // Left grip is the reserved exit chord and has no shell action.
            // Keeping it held must not postpone returning to the right-hand ray.
            if hands.is_some_and(|hands| hands.right.neutral()) {
                self.rearm = false;
            }
            return consumed;
        };
        let Some(hands) = hands else {
            self.stop();
            return true;
        };
        if hands.left.grip >= 0.75 {
            capture.exit_started.get_or_insert(now);
        } else if hands.left.grip < 0.35 {
            capture.exit_started = None;
        }
        if capture.exit_started.is_some_and(|started| {
            now.saturating_duration_since(started) >= Duration::from_millis(800)
        }) {
            self.stop();
            return true;
        }
        match capture.controller.mode() {
            GamepadMode::Pending(id) if id == capture.generation => {}
            GamepadMode::Active(id) if id == capture.generation => {
                capture.armed |= hands.neutral();
                let state = if capture.armed {
                    hands.state()
                } else {
                    GamepadState::default()
                };
                if !capture.controller.sample(id, state, now) {
                    self.stop();
                }
            }
            _ => self.stop(),
        }
        true
    }
}
impl Drop for Mode {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    use weld_client::{
        ClientAdapter, ClientEventQueue, ClientProvenance, ClientSourceDescriptor, ClientSourceId,
    };
    use weld_hoist_core::{
        DestinationPortCommand, DestinationPortEvent, DestinationPortRecord,
        DestinationRelayAdapter, HoistDestinationPort, HoistPortResult, HoistSessionId,
        gamepad::{GamepadCaptureState, GamepadStatus},
    };

    #[derive(Default)]
    struct PortState {
        inbound: Vec<DestinationPortRecord>,
        outbound: Vec<DestinationPortCommand>,
    }
    struct Port(Rc<RefCell<PortState>>);
    impl HoistDestinationPort for Port {
        fn poll(&mut self) -> HoistPortResult<Vec<DestinationPortRecord>> {
            Ok(std::mem::take(&mut self.0.borrow_mut().inbound))
        }
        fn submit(&mut self, command: DestinationPortCommand) -> HoistPortResult<()> {
            self.0.borrow_mut().outbound.push(command);
            Ok(())
        }
        fn disconnect(&mut self) {}
    }
    #[test]
    fn capture_waits_for_ack_and_neutral_then_hold_exit_releases_through_relay() {
        let controller = GamepadController::default();
        let port = Rc::new(RefCell::new(PortState::default()));
        let mut relay = DestinationRelayAdapter::new(
            ClientSourceId::new(0),
            ClientSourceDescriptor::new(ClientSourceId::new(1), ClientProvenance::Relocated),
            Port(port.clone()),
        )
        .with_gamepad(Some(controller.clone()));
        let feedback = |status| {
            port.borrow_mut().inbound.push(DestinationPortRecord {
                session: HoistSessionId::new(0),
                event: DestinationPortEvent::Gamepad(status),
            })
        };
        feedback(GamepadStatus::Available);
        relay.drain_events(&mut ClientEventQueue::default());
        let mut mode = Mode::default();
        assert!(mode.enter(controller.clone()));
        let now = Instant::now();
        assert!(mode.step(Some(Hands::default()), now));
        assert_eq!(controller.mode(), GamepadMode::Pending(1));
        relay.drain_events(&mut ClientEventQueue::default());
        assert_eq!(
            port.borrow().outbound.len(),
            1,
            "only Begin before acknowledgement"
        );
        feedback(GamepadStatus::Capture {
            generation: 1,
            state: GamepadCaptureState::Active,
        });
        relay.drain_events(&mut ClientEventQueue::default());
        let held = Hands {
            right: Hand {
                a: true,
                ..Default::default()
            },
            ..Default::default()
        };
        mode.step(Some(held), now);
        assert!(!mode.capture.as_ref().expect("capture").armed);
        mode.step(Some(Hands::default()), now);
        assert!(mode.capture.as_ref().expect("capture").armed);
        mode.step(Some(held), now);
        let exiting = Hands {
            left: Hand {
                grip: 1.0,
                ..Default::default()
            },
            ..held
        };
        mode.step(Some(exiting), now);
        mode.step(Some(exiting), now + Duration::from_millis(800));
        assert!(!mode.active());
        assert_eq!(controller.mode(), GamepadMode::Ready);
        relay.drain_events(&mut ClientEventQueue::default());
        assert_eq!(
            port.borrow().outbound.len(),
            2,
            "queued states replaced by out-of-band End"
        );
        assert!(mode.step(Some(held), now));
        assert!(mode.step(Some(Hands::default()), now));
        assert!(!mode.step(Some(held), now));
        assert!(mode.enter(controller.clone()));
        mode.step(None, now);
        assert!(!mode.active(), "tracking loss exits pending capture too");
    }
    #[test]
    fn physical_mapping_preserves_analog_range_and_button_positions() {
        let hands = Hands {
            left: Hand {
                stick: [0.0, 1.0],
                a: true,
                click: true,
                ..Default::default()
            },
            right: Hand {
                a: true,
                b: true,
                trigger: 1.0,
                ..Default::default()
            },
        };
        let state = hands.state();
        assert_eq!(state.left, [0, -32767]);
        assert!(
            state.buttons.west && state.buttons.south && state.buttons.east && state.buttons.select
        );
        assert!(state.buttons.right_shoulder);
        assert_eq!(state.triggers, [0, u16::MAX]);
        assert_eq!(stick([0.03, -0.04]), [0, 0]);
        let gripping = Hands {
            right: Hand {
                grip: 1.0,
                ..hands.right
            },
            ..hands
        };
        assert_eq!(
            gripping.state(),
            state,
            "right grip is mouse-only in gameplay"
        );
    }
    #[test]
    fn rearm_waits_for_shell_controls_and_never_reenters_automatically() {
        let mut mode = Mode {
            capture: None,
            rearm: true,
        };
        let now = Instant::now();
        let held = Hands {
            right: Hand {
                a: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(mode.step(Some(held), now));
        assert!(mode.step(None, now));
        assert!(mode.step(Some(Hands::default()), now));
        assert!(!mode.step(Some(held), now));
        assert!(!mode.active());
    }

    #[test]
    fn held_left_exit_grip_does_not_block_shell_rearming() {
        let mut mode = Mode {
            capture: None,
            rearm: true,
        };
        let hands = Hands {
            left: Hand {
                grip: 1.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let now = Instant::now();
        assert!(mode.step(Some(hands), now));
        assert!(!mode.step(Some(hands), now));
        assert!(!mode.active());
    }
}
