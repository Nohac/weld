//! Physical XR observations are separate from mailbox admission. A rejected
//! press can retry while held/on-target, but never after withdrawal or release.
pub(super) const BUTTONS: [i64; 3] = [1, 3, 2]; // Left, middle, right mouse buttons.
const PRESS: f32 = 0.75;
const RELEASE: f32 = 0.35;
const DEADZONE: f64 = 0.2;

#[derive(Default)]
pub(super) struct Policy {
    token: Option<(u64, u64)>,
    physical: [bool; 3],
    pending: [bool; 3],
    wheel_armed: bool,
    last_sample: f64,
    scrolling: bool,
}

#[derive(Default, Debug)]
pub(super) struct Actions {
    pub reset: bool,
    pub edges: [Option<bool>; 3],
    /// Continuous vertical distance; zero ends the gesture.
    pub wheel: Option<f64>,
}

impl Policy {
    /// Gameplay reserves A/B, trigger and stick for the gamepad. Right grip
    /// alone drives the mouse, with the same admission, drag and regain rules.
    pub fn step_gamepad(&mut self, token: (u64, u64), hit: bool, grip: f32, now: f64) -> Actions {
        self.step(token, hit, [grip, 0.0], false, 0.0, now)
    }
    pub fn deactivate(&mut self) -> bool {
        let active = self.token.take().is_some();
        self.physical = [false; 3];
        self.pending = [false; 3];
        self.wheel_armed = false;
        self.scrolling = false;
        active
    }
    pub fn step(
        &mut self,
        token: (u64, u64),
        hit: bool,
        analog: [f32; 2],
        click: bool,
        axis: f64,
        now: f64,
    ) -> Actions {
        if !analog.into_iter().all(f32::is_finite) || !axis.is_finite() || !now.is_finite() {
            return Actions {
                reset: self.deactivate(),
                ..Actions::default()
            };
        }
        if self.token != Some(token) {
            self.token = Some(token);
            // Treat anything above the release threshold as already held on
            // startup/regain; it must be released before becoming a new press.
            self.physical = [analog[0] > RELEASE, analog[1] > RELEASE, click];
            self.pending = [false; 3];
            self.wheel_armed = axis.abs() <= DEADZONE;
            self.last_sample = now;
            self.scrolling = false;
            return Actions {
                reset: true,
                ..Actions::default()
            };
        }
        let mut actions = Actions::default();
        let current = [
            analog[0] >= if self.physical[0] { RELEASE } else { PRESS },
            analog[1] >= if self.physical[1] { RELEASE } else { PRESS },
            click,
        ];
        for (index, down) in current.into_iter().enumerate() {
            if down != self.physical[index] {
                self.physical[index] = down;
                self.pending[index] = down && hit;
                if !down {
                    actions.edges[index] = Some(false);
                }
            }
            if !hit {
                self.pending[index] = false;
            }
            if self.pending[index] {
                actions.edges[index] = Some(true);
            }
        }
        if axis.abs() <= DEADZONE {
            self.wheel_armed = true;
        }
        let elapsed = (now - self.last_sample).clamp(0.0, 1.0 / 30.0);
        self.last_sample = now;
        let speed = ((axis.abs().min(1.0) - DEADZONE) / (1.0 - DEADZONE)).max(0.0);
        if hit && self.wheel_armed && speed > 0.0 {
            actions.wheel = Some(-axis.signum() * speed * speed * 1200.0 * elapsed);
            self.scrolling = true;
        } else if self.scrolling {
            actions.wheel = Some(0.0);
            self.scrolling = false;
        }
        actions
    }
    pub fn admitted(&mut self, index: usize, accepted: bool) {
        if accepted {
            self.pending[index] = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn neutral(policy: &mut Policy) {
        policy.step((1, 0), true, [0.0; 2], false, 0.0, 0.0);
    }
    #[test]
    fn gamepad_grip_drags_and_releases_off_target_without_other_mouse_actions() {
        let mut policy = Policy::default();
        assert!(policy.step_gamepad((1, 0), true, 0.0, 0.0).reset);
        let press = policy.step_gamepad((1, 0), true, 1.0, 0.1);
        assert_eq!(press.edges, [Some(true), None, None]);
        assert_eq!(press.wheel, None);
        policy.admitted(0, true);
        let drag = policy.step_gamepad((1, 0), false, 0.5, 0.2);
        assert_eq!(drag.edges, [None; 3]);
        assert_eq!(drag.wheel, None);
        assert_eq!(
            policy.step_gamepad((1, 0), false, 0.0, 0.3).edges,
            [Some(false), None, None]
        );
    }
    #[test]
    fn gamepad_pointer_reset_and_surface_change_require_grip_release() {
        let mut policy = Policy::default();
        for token in [(1, 0), (2, 0)] {
            assert!(policy.step_gamepad(token, true, 1.0, 0.0).reset);
            assert_eq!(policy.step_gamepad(token, true, 1.0, 0.1).edges, [None; 3]);
            policy.step_gamepad(token, true, 0.0, 0.2);
            assert_eq!(
                policy.step_gamepad(token, true, 1.0, 0.3).edges,
                [Some(true), None, None]
            );
            policy.admitted(0, true);
        }
        assert!(
            policy.deactivate(),
            "exit/lost tracking must request remote reset"
        );
        assert!(policy.step_gamepad((2, 0), true, 1.0, 0.4).reset);
        assert_eq!(policy.step_gamepad((2, 0), true, 1.0, 0.5).edges, [None; 3]);
    }
    #[test]
    fn failed_press_retries_until_admission_then_never_duplicates() {
        let mut policy = Policy::default();
        neutral(&mut policy);
        for time in [0.01, 0.02] {
            assert_eq!(
                policy
                    .step((1, 0), true, [1.0, 0.0], false, 0.0, time)
                    .edges[0],
                Some(true)
            );
            policy.admitted(0, false);
        }
        policy.admitted(0, true);
        assert_eq!(
            policy
                .step((1, 0), false, [1.0, 0.0], false, 0.0, 0.03)
                .edges,
            [None; 3]
        );
        assert_eq!(
            policy.step((1, 0), false, [0.0; 2], false, 0.0, 0.04).edges[0],
            Some(false)
        );
    }
    #[test]
    fn off_target_press_and_pending_withdrawal_require_release() {
        for initially_hit in [true, false] {
            let mut policy = Policy::default();
            neutral(&mut policy);
            policy.step((1, 0), initially_hit, [1.0, 0.0], false, 0.0, 0.01);
            assert_eq!(
                policy
                    .step((1, 0), false, [1.0, 0.0], false, 0.0, 0.02)
                    .edges[0],
                None
            );
            assert_eq!(
                policy
                    .step((1, 0), true, [1.0, 0.0], false, 0.0, 0.03)
                    .edges[0],
                None
            );
            assert_eq!(
                policy.step((1, 0), true, [0.0; 2], false, 0.0, 0.04).edges[0],
                Some(false)
            );
        }
    }
    #[test]
    fn repeated_rejection_is_bounded_and_release_allows_next_press() {
        let mut policy = Policy::default();
        neutral(&mut policy);
        for tick in 1..100 {
            assert_eq!(
                policy
                    .step((1, 0), true, [1.0, 1.0], true, 0.0, f64::from(tick))
                    .edges,
                [Some(true); 3]
            );
        }
        assert_eq!(
            policy.step((1, 0), true, [0.0; 2], false, 0.0, 100.0).edges,
            [Some(false); 3]
        );
        assert_eq!(
            policy
                .step((1, 0), true, [1.0, 0.0], false, 0.0, 101.0)
                .edges[0],
            Some(true)
        );
    }
    #[test]
    fn hysteresis_and_regain_do_not_replay_held_controls() {
        let mut policy = Policy::default();
        neutral(&mut policy);
        policy.step((1, 0), true, [0.8, 0.8], false, 0.0, 0.1);
        policy.admitted(0, true);
        policy.admitted(1, true);
        assert_eq!(
            policy.step((1, 0), true, [0.5, 0.5], false, 0.0, 0.2).edges,
            [None; 3]
        );
        assert!(policy.deactivate());
        assert!(!policy.deactivate());
        assert!(policy.step((1, 0), true, [0.5, 0.5], true, 1.0, 1.0).reset);
        assert_eq!(
            policy.step((1, 0), true, [0.8, 0.8], true, 1.0, 1.1).edges,
            [None; 3]
        );
        assert_eq!(
            policy.step((1, 1), true, [0.8, 0.8], true, 1.0, 1.2).edges,
            [None; 3]
        );
        assert_eq!(
            policy.step((1, 1), true, [0.1, 0.1], false, 0.0, 1.3).edges,
            [Some(false); 3]
        );
    }
    #[test]
    fn scroll_speed_depends_on_deflection_and_elapsed_time_without_catchup() {
        let mut policy = Policy::default();
        neutral(&mut policy);
        assert_eq!(
            policy.step((1, 0), true, [0.0; 2], false, 0.1, 0.01).wheel,
            None
        );
        assert_eq!(
            policy.step((1, 0), true, [0.0; 2], false, 1.0, 0.02).wheel,
            Some(-12.0)
        );
        policy.step((1, 0), true, [0.0; 2], false, 0.0, 0.03);
        let reverse = policy
            .step((1, 0), true, [0.0; 2], false, -1.0, 0.04)
            .wheel
            .expect("reverse");
        assert!((reverse - 12.0).abs() < 1e-9);
        assert_eq!(
            policy.step((1, 0), true, [0.0; 2], false, -1.0, 10.0).wheel,
            Some(40.0)
        );
        let fine = policy
            .step((1, 0), true, [0.0; 2], false, -0.6, 10.01)
            .wheel
            .expect("fine scroll");
        assert!((fine - 3.0).abs() < 1e-9);
        assert_eq!(
            policy.step((1, 0), false, [0.0; 2], false, 1.0, 11.0).wheel,
            Some(0.0)
        );
    }
    #[test]
    fn smooth_scroll_distance_is_independent_of_refresh_rate() {
        for hz in [60, 72, 90, 120] {
            let mut policy = Policy::default();
            neutral(&mut policy);
            let distance: f64 = (1..=hz)
                .map(|tick| {
                    policy
                        .step(
                            (1, 0),
                            true,
                            [0.0; 2],
                            false,
                            0.6,
                            f64::from(tick) / f64::from(hz),
                        )
                        .wheel
                        .unwrap_or(0.0)
                })
                .sum();
            assert!((distance + 300.0).abs() < 1e-8);
        }
    }
}
