//! Immutable permissions belong to one revocable control handle.

use crate::{InputAction, Operation};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    View,
    Keyboard,
    Pointer,
    Resize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Capabilities {
    pub view: bool,
    pub keyboard: bool,
    pub pointer: bool,
    pub resize: bool,
}

impl Capabilities {
    pub const ALL: Self = Self {
        view: true,
        keyboard: true,
        pointer: true,
        resize: true,
    };

    pub const fn allows(self, capability: Capability) -> bool {
        match capability {
            Capability::View => self.view,
            Capability::Keyboard => self.keyboard,
            Capability::Pointer => self.pointer,
            Capability::Resize => self.resize,
        }
    }

    pub fn toggle(&mut self, capability: Capability) {
        let value = match capability {
            Capability::View => &mut self.view,
            Capability::Keyboard => &mut self.keyboard,
            Capability::Pointer => &mut self.pointer,
            Capability::Resize => &mut self.resize,
        };
        *value = !*value;
    }
}

impl Operation {
    pub(crate) fn allowed_by(&self, capabilities: Capabilities) -> bool {
        let actions_allowed = |actions: &[InputAction]| {
            actions.iter().all(|action| match action {
                InputAction::Text(_) | InputAction::Keys { .. } => capabilities.keyboard,
                InputAction::Move { .. }
                | InputAction::Button { .. }
                | InputAction::Click { .. }
                | InputAction::Scroll { .. } => capabilities.pointer,
                InputAction::Wait { .. } | InputAction::At { .. } => true,
            })
        };
        let capability = match self {
            Self::Sequence(input) => return actions_allowed(&input.actions),
            Self::RecordInput { input, .. } => {
                return capabilities.view && actions_allowed(&input.actions);
            }
            Self::Capture | Self::CaptureAfter { .. } | Self::Record(_) => Capability::View,
            Self::Keys { .. } | Self::Chord { .. } | Self::TypeText { .. } => Capability::Keyboard,
            Self::Pointer { .. }
            | Self::Click { .. }
            | Self::Button { .. }
            | Self::Scroll { .. } => Capability::Pointer,
            Self::Resize { .. } | Self::Scale { .. } => Capability::Resize,
        };
        capabilities.allows(capability)
    }
}
