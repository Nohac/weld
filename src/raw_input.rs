//! Lossless, backend-neutral seat input owned by Weld.
//!
//! Nested Winit and future libinput adapters produce these values. They retain
//! protocol ordering and Linux codes independently of the Bevy/Leafwing action
//! projection and never contain Smithay resources.

use bevy::{
    input::{ButtonState, keyboard::Key},
    math::Vec2,
};

/// A position in Weld's compositor coordinate space.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct InputPosition {
    pub x: f64,
    pub y: f64,
}

impl InputPosition {
    pub(crate) const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub(crate) fn as_vec2(self) -> Vec2 {
        Vec2::new(self.x as f32, self.y as f32)
    }
}

/// A Linux evdev keyboard code, without XKB's protocol offset of eight.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct LinuxKeycode(pub u32);

/// A Linux input-event mouse button code such as `BTN_LEFT`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct LinuxButtonCode(pub u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RawScrollSource {
    Wheel,
    Finger,
    Continuous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RawScrollPhase {
    Started,
    Moved,
    Ended,
    Cancelled,
}

/// Scroll data using Wayland/libinput axis direction and high-resolution wheel units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RawScrollFrame {
    pub source: RawScrollSource,
    pub phase: RawScrollPhase,
    pub horizontal: f64,
    pub vertical: f64,
    pub horizontal_v120: Option<i32>,
    pub vertical_v120: Option<i32>,
    pub horizontal_stop: bool,
    pub vertical_stop: bool,
}

/// One ordered input transition from a nested or standalone seat backend.
///
/// The initial compositor exposes one logical seat, matching Winit's nested
/// host pointer. Multi-seat support should add a stable seat identifier here;
/// independent cursors additionally require a device/pointer identifier,
/// because several physical devices may belong to the same logical seat.
///
/// Physical [`bevy::input::keyboard::KeyCode`] values are deliberately derived
/// later from [`LinuxKeycode`], so every Linux backend shares one mapping. The
/// optional logical key is adapter-supplied because it depends on the active
/// host or Weld-owned XKB keymap. Text input remains deferred with that keymap
/// work, and nested input currently drops unidentifiable Winit scancodes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawSeatEvent {
    pub event: RawSeatEventKind,
    pub time: u32,
}

impl RawSeatEvent {
    pub(crate) const fn new(event: RawSeatEventKind, time: u32) -> Self {
        Self { event, time }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RawSeatEventKind {
    PointerMotion {
        position: InputPosition,
    },
    PointerLeft {
        position: InputPosition,
    },
    PointerButton {
        position: Option<InputPosition>,
        button: LinuxButtonCode,
        state: ButtonState,
    },
    PointerAxis {
        position: Option<InputPosition>,
        axis: RawScrollFrame,
    },
    Keyboard {
        keycode: LinuxKeycode,
        logical_key: Option<Key>,
        state: ButtonState,
    },
    HostFocusLost,
}
