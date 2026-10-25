//! Positional action encoding shared by CLI, RPC and the MCP schema.

use super::{InputAction, InputKey, KeyState};
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
        }
    }
}
