//! Connection-scoped standard gamepad input, independent of window focus.
use serde::{Deserialize, Serialize};

use crate::HoistSessionId;

/// Surface sessions start at one. Zero is reserved for connection control.
pub const GAMEPAD_SESSION: HoistSessionId = HoistSessionId::new(0);

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum HatDirection {
    Negative,
    #[default]
    Center,
    Positive,
}
impl HatDirection {
    pub const fn value(self) -> i32 {
        match self {
            Self::Negative => -1,
            Self::Center => 0,
            Self::Positive => 1,
        }
    }
}

/// Positional button names avoid assuming Nintendo or Xbox face labels.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GamepadButtons {
    pub south: bool,
    pub east: bool,
    pub west: bool,
    pub north: bool,
    pub left_shoulder: bool,
    pub right_shoulder: bool,
    pub select: bool,
    pub start: bool,
    pub guide: bool,
    pub left_stick: bool,
    pub right_stick: bool,
}

/// A complete state. Signed sticks use positive Y down; triggers rest at zero.
/// Integer axes and typed hats exclude NaNs and out-of-range wire values.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GamepadState {
    pub buttons: GamepadButtons,
    pub left: [i16; 2],
    pub right: [i16; 2],
    pub triggers: [u16; 2],
    pub dpad: [HatDirection; 2],
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum GamepadRequest {
    Begin {
        generation: u64,
    },
    State {
        generation: u64,
        state: GamepadState,
    },
    End {
        generation: u64,
    },
}
impl GamepadRequest {
    pub const fn generation(self) -> u64 {
        match self {
            Self::Begin { generation }
            | Self::State { generation, .. }
            | Self::End { generation } => generation,
        }
    }
    /// Only adjacent analog updates may replace each other. Button, hat and
    /// trigger threshold transitions are barriers, just like Begin and End.
    pub fn supersedes(self, previous: Self) -> bool {
        match (previous, self) {
            (
                Self::State {
                    generation: old,
                    state: a,
                },
                Self::State {
                    generation,
                    state: b,
                },
            ) => {
                old == generation
                    && a.buttons == b.buttons
                    && a.dpad == b.dpad
                    && a.triggers.map(|value| value >= 32768)
                        == b.triggers.map(|value| value >= 32768)
            }
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum GamepadStatus {
    Available,
    Capture {
        generation: u64,
        state: GamepadCaptureState,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum GamepadCaptureState {
    Active,
    Stopped,
    Denied,
    TimedOut,
    DeviceFailed,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_hat_is_rejected_at_input_boundary() {
        assert!(postcard::from_bytes::<HatDirection>(&[3]).is_err());
    }
    #[test]
    fn analog_replacement_preserves_discrete_and_lifecycle_edges() {
        let first = GamepadRequest::State {
            generation: 1,
            state: GamepadState::default(),
        };
        let mut state = GamepadState {
            left: [100, 200],
            ..Default::default()
        };
        assert!(
            GamepadRequest::State {
                generation: 1,
                state
            }
            .supersedes(first)
        );
        state.buttons.south = true;
        assert!(
            !GamepadRequest::State {
                generation: 1,
                state
            }
            .supersedes(first)
        );
        assert!(!GamepadRequest::End { generation: 1 }.supersedes(first));
        assert!(
            !GamepadRequest::State {
                generation: 2,
                state: GamepadState::default()
            }
            .supersedes(first)
        );
    }
}
