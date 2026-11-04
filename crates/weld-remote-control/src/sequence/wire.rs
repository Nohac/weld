//! Positional action encoding shared by CLI, RPC and the MCP schema.

use super::{InputAction, InputKey, KeyState};
use crate::{Easing, MouseButton};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
pub enum Text {
    #[serde(rename = "t")]
    Tag,
}
#[derive(Serialize, Deserialize, JsonSchema)]
pub enum Press {
    #[serde(rename = "p")]
    Tag,
}
#[derive(Serialize, Deserialize, JsonSchema)]
pub enum Down {
    #[serde(rename = "d")]
    Tag,
}
#[derive(Serialize, Deserialize, JsonSchema)]
pub enum Up {
    #[serde(rename = "u")]
    Tag,
}
#[derive(Serialize, Deserialize, JsonSchema)]
pub enum Wait {
    #[serde(rename = "w")]
    Tag,
}
#[derive(Serialize, Deserialize, JsonSchema)]
pub enum At {
    #[serde(rename = "at")]
    Tag,
}
#[derive(Serialize, Deserialize, JsonSchema)]
pub enum Move {
    #[serde(rename = "m")]
    Tag,
}
#[derive(Serialize, Deserialize, JsonSchema)]
pub enum Button {
    #[serde(rename = "b")]
    Tag,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub enum Click {
    #[serde(rename = "click")]
    Tag,
}
#[derive(Serialize, Deserialize, JsonSchema)]
pub enum Scroll {
    #[serde(rename = "scroll")]
    Tag,
}
#[derive(Serialize, Deserialize, JsonSchema)]
pub enum Wheel {
    #[serde(rename = "wheel")]
    Tag,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Keys {
    One(InputKey),
    Group(Vec<InputKey>),
}

impl From<Keys> for Vec<InputKey> {
    fn from(keys: Keys) -> Self {
        match keys {
            Keys::One(key) => vec![key],
            Keys::Group(keys) => keys,
        }
    }
}

impl From<Vec<InputKey>> for Keys {
    fn from(mut keys: Vec<InputKey>) -> Self {
        if keys.len() == 1
            && let Some(key) = keys.pop()
        {
            Self::One(key)
        } else {
            Self::Group(keys)
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Action {
    /// ["t", "literal Unicode text"]
    Text(Text, String),
    /// ["p", key_or_keys, hold_ms?, gap_ms?]
    #[schemars(extend("minItems" = 2))]
    Press(
        Press,
        Keys,
        #[serde(default)] Option<u32>,
        #[serde(default)] Option<u32>,
    ),
    /// ["d", key_or_keys, gap_ms?]
    #[schemars(extend("minItems" = 2))]
    Down(Down, Keys, #[serde(default)] Option<u32>),
    /// ["u", key_or_keys, gap_ms?]
    #[schemars(extend("minItems" = 2))]
    Up(Up, Keys, #[serde(default)] Option<u32>),
    /// ["w", milliseconds]
    Wait(Wait, u32),
    /// ["at", milliseconds_from_start]
    At(At, u32),
    /// ["m", x, y, duration_ms?, easing?]
    #[schemars(extend("minItems" = 3))]
    Move(
        Move,
        f64,
        f64,
        #[serde(default)] Option<u32>,
        #[serde(default)] Option<Easing>,
    ),
    /// ["b", button_name_or_code, pressed]
    Button(Button, MouseButton, bool),
    /// ["click", button_name_or_code, count?, interval_ms?] at the current pointer.
    #[schemars(extend("minItems" = 2))]
    Click(
        Click,
        MouseButton,
        #[serde(default)] Option<u32>,
        #[serde(default)] Option<u32>,
    ),
    /// ["wheel", horizontal_notches, vertical_notches, count?, interval_ms?]. Positive is right/down.
    #[schemars(extend("minItems" = 3))]
    Wheel(
        Wheel,
        f64,
        f64,
        #[serde(default)] Option<u32>,
        #[serde(default)] Option<u32>,
    ),
    /// ["scroll", horizontal_pixels, vertical_pixels, count?, interval_ms?]. Each repeat is a completed gesture.
    #[schemars(extend("minItems" = 3))]
    Scroll(
        Scroll,
        f64,
        f64,
        #[serde(default)] Option<u32>,
        #[serde(default)] Option<u32>,
    ),
}

impl From<Action> for InputAction {
    fn from(action: Action) -> Self {
        match action {
            Action::Text(_, text) => Self::Text(text),
            Action::Press(_, keys, hold_ms, delay_ms) => Self::Keys {
                keys: keys.into(),
                state: KeyState::Press,
                delay_ms,
                hold_ms,
            },
            Action::Down(_, keys, delay_ms) => Self::Keys {
                keys: keys.into(),
                state: KeyState::Down,
                delay_ms,
                hold_ms: None,
            },
            Action::Up(_, keys, delay_ms) => Self::Keys {
                keys: keys.into(),
                state: KeyState::Up,
                delay_ms,
                hold_ms: None,
            },
            Action::Wait(_, wait_ms) => Self::Wait { wait_ms },
            Action::At(_, at_ms) => Self::At { at_ms },
            Action::Move(_, x, y, duration_ms, easing) => Self::Move {
                x,
                y,
                duration_ms: duration_ms.unwrap_or(0),
                easing: easing.unwrap_or_default(),
            },
            Action::Button(_, button, pressed) => Self::Button {
                button: button.code(),
                pressed,
            },
            Action::Click(_, button, count, interval) => Self::Click {
                button: button.code(),
                count: count.unwrap_or(1),
                interval_ms: interval.unwrap_or(0),
            },
            Action::Wheel(_, horizontal, vertical, count, interval) => Self::Scroll {
                horizontal,
                vertical,
                wheel: true,
                count: count.unwrap_or(1),
                interval_ms: interval.unwrap_or(0),
            },
            Action::Scroll(_, horizontal, vertical, count, interval) => Self::Scroll {
                horizontal,
                vertical,
                wheel: false,
                count: count.unwrap_or(1),
                interval_ms: interval.unwrap_or(0),
            },
        }
    }
}

impl From<InputAction> for Action {
    fn from(action: InputAction) -> Self {
        match action {
            InputAction::Text(text) => Self::Text(Text::Tag, text),
            InputAction::Keys {
                keys,
                state: KeyState::Press,
                delay_ms,
                hold_ms,
            } => Self::Press(Press::Tag, keys.into(), hold_ms, delay_ms),
            InputAction::Keys {
                keys,
                state: KeyState::Down,
                delay_ms,
                ..
            } => Self::Down(Down::Tag, keys.into(), delay_ms),
            InputAction::Keys {
                keys,
                state: KeyState::Up,
                delay_ms,
                ..
            } => Self::Up(Up::Tag, keys.into(), delay_ms),
            InputAction::Wait { wait_ms } => Self::Wait(Wait::Tag, wait_ms),
            InputAction::At { at_ms } => Self::At(At::Tag, at_ms),
            InputAction::Move {
                x,
                y,
                duration_ms,
                easing,
            } => Self::Move(Move::Tag, x, y, Some(duration_ms), Some(easing)),
            InputAction::Click {
                button,
                count,
                interval_ms,
            } => Self::Click(
                Click::Tag,
                MouseButton::Code(button),
                Some(count),
                Some(interval_ms),
            ),
            InputAction::Scroll {
                horizontal,
                vertical,
                wheel,
                count,
                interval_ms,
            } => {
                if wheel {
                    Self::Wheel(
                        Wheel::Tag,
                        horizontal,
                        vertical,
                        Some(count),
                        Some(interval_ms),
                    )
                } else {
                    Self::Scroll(
                        Scroll::Tag,
                        horizontal,
                        vertical,
                        Some(count),
                        Some(interval_ms),
                    )
                }
            }
            InputAction::Button { button, pressed } => {
                Self::Button(Button::Tag, MouseButton::Code(button), pressed)
            }
        }
    }
}
