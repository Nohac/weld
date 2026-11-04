//! Pointer policy shared by immediate operations and scheduled input actions.

use crate::Operation;
use anyhow::{Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ButtonName {
    Left,
    Right,
    Middle,
    Side,
    Extra,
    Forward,
    Back,
    Task,
}

/// Linux evdev button code or a discoverable name (including `middle`).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum MouseButton {
    Name(ButtonName),
    Code(u32),
}
impl MouseButton {
    pub fn code(self) -> u32 {
        match self {
            Self::Code(code) => code,
            Self::Name(name) => match name {
                ButtonName::Left => 272,
                ButtonName::Right => 273,
                ButtonName::Middle => 274,
                ButtonName::Side => 275,
                ButtonName::Extra => 276,
                ButtonName::Forward => 277,
                ButtonName::Back => 278,
                ButtonName::Task => 279,
            },
        }
    }
}
impl std::str::FromStr for MouseButton {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let button = match value.parse::<u32>() {
            Ok(code) => Self::Code(code),
            Err(_) => Self::Name(
                serde_json::from_value(serde_json::Value::String(value.into()))
                    .map_err(|error| error.to_string())?,
            ),
        };
        validate_button(button.code()).map_err(|error| error.to_string())?;
        Ok(button)
    }
}

pub(crate) fn validate_button(button: u32) -> Result<()> {
    ensure!((272..=279).contains(&button), "invalid pointer button");
    Ok(())
}

pub(crate) fn validate_scroll(horizontal: f64, vertical: f64) -> Result<()> {
    ensure!(
        horizontal.is_finite()
            && vertical.is_finite()
            && horizontal.abs() <= 1_000_000.
            && vertical.abs() <= 1_000_000.,
        "invalid scroll delta"
    );
    Ok(())
}

/// Curves map elapsed time to progress; coordinates always finish at the endpoint.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Easing {
    #[default]
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
}
impl std::str::FromStr for Easing {
    type Err = serde_json::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        serde_json::from_value(serde_json::Value::String(value.into()))
    }
}
impl Easing {
    pub(crate) fn sample(self, progress: f64) -> f64 {
        let t = progress.clamp(0., 1.);
        match self {
            Self::Linear => t,
            Self::EaseIn => t * t,
            Self::EaseOut => 1. - (1. - t) * (1. - t),
            Self::EaseInOut if t < 0.5 => 2. * t * t,
            Self::EaseInOut => 1. - 2. * (1. - t) * (1. - t),
        }
    }
}

pub(crate) enum PointerAction {
    Click(u32),
    Scroll {
        horizontal: f64,
        vertical: f64,
        wheel: bool,
    },
}
impl PointerAction {
    pub fn at(self, x: f64, y: f64) -> Operation {
        match self {
            Self::Click(button) => Operation::Click { x, y, button },
            Self::Scroll {
                horizontal,
                vertical,
                wheel,
            } => Operation::Scroll {
                x,
                y,
                horizontal,
                vertical,
                wheel,
            },
        }
    }
}
