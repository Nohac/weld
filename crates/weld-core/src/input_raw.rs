//! Backend-neutral seat input shared across Weld's input pipeline.
//!
//! Nested Winit and standalone libinput sources produce these values. ECS
//! projection and Smithay delivery consume them without introducing host or
//! protocol resources into the vocabulary.

use winit::keyboard::Key;

/// Press/release state shared by all host input adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ButtonState {
    Pressed,
    Released,
}

/// A position in Weld's compositor coordinate space.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InputPosition {
    pub x: f64,
    pub y: f64,
}

impl InputPosition {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// A relative two-dimensional touchpad gesture delta.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InputDelta {
    pub x: f64,
    pub y: f64,
}

impl InputDelta {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// A Linux evdev keyboard code, without XKB's protocol offset of eight.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LinuxKeycode(pub u32);

/// A Linux input-event mouse button code such as `BTN_LEFT`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LinuxButtonCode(pub u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawScrollSource {
    Wheel,
    Finger,
    Continuous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawScrollPhase {
    Started,
    Moved,
    Ended,
    Cancelled,
}

/// Scroll data using Wayland/libinput axis direction and high-resolution wheel units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RawScrollFrame {
    pub source: RawScrollSource,
    pub phase: RawScrollPhase,
    pub horizontal: f64,
    pub vertical: f64,
    pub horizontal_v120: Option<i32>,
    pub vertical_v120: Option<i32>,
    pub horizontal_stop: bool,
    pub vertical_stop: bool,
}

impl RawScrollFrame {
    pub const fn cancelled_finger(horizontal_stop: bool, vertical_stop: bool) -> Self {
        Self {
            source: RawScrollSource::Finger,
            phase: RawScrollPhase::Cancelled,
            horizontal: 0.0,
            vertical: 0.0,
            horizontal_v120: None,
            vertical_v120: None,
            horizontal_stop,
            vertical_stop,
        }
    }
}

/// A full-fidelity touchpad gesture independent of libinput and Wayland types.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerGesture {
    Swipe(TouchpadSwipe),
    Pinch(TouchpadPinch),
    Hold(TouchpadHold),
}

impl PointerGesture {
    pub const fn kind(self) -> PointerGestureKind {
        match self {
            Self::Swipe(_) => PointerGestureKind::Swipe,
            Self::Pinch(_) => PointerGestureKind::Pinch,
            Self::Hold(_) => PointerGestureKind::Hold,
        }
    }

    pub const fn is_begin(self) -> bool {
        matches!(
            self,
            Self::Swipe(TouchpadSwipe::Begin { .. })
                | Self::Pinch(TouchpadPinch::Begin { .. })
                | Self::Hold(TouchpadHold::Begin { .. })
        )
    }

    pub const fn is_end(self) -> bool {
        matches!(
            self,
            Self::Swipe(TouchpadSwipe::End { .. })
                | Self::Pinch(TouchpadPinch::End { .. })
                | Self::Hold(TouchpadHold::End { .. })
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerGestureKind {
    Swipe,
    Pinch,
    Hold,
}

impl PointerGestureKind {
    pub const fn cancelled(self) -> PointerGesture {
        match self {
            Self::Swipe => PointerGesture::Swipe(TouchpadSwipe::End { cancelled: true }),
            Self::Pinch => PointerGesture::Pinch(TouchpadPinch::End { cancelled: true }),
            Self::Hold => PointerGesture::Hold(TouchpadHold::End { cancelled: true }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TouchpadSwipe {
    Begin { fingers: u32 },
    Update { delta: InputDelta },
    End { cancelled: bool },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TouchpadPinch {
    Begin {
        fingers: u32,
    },
    Update {
        delta: InputDelta,
        /// Scale relative to the beginning of this pinch gesture.
        scale: f64,
        /// Rotation in degrees relative to the previous update.
        rotation: f64,
    },
    End {
        cancelled: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TouchpadHold {
    Begin { fingers: u32 },
    End { cancelled: bool },
}

/// One ordered input transition from a nested or standalone seat backend.
///
/// The initial compositor exposes one logical seat, matching Winit's nested
/// host pointer. Multi-seat support should add a stable seat identifier here;
/// independent cursors additionally require a device/pointer identifier,
/// because several physical devices may belong to the same logical seat.
///
/// Bevy physical key codes are deliberately derived
/// later from [`LinuxKeycode`], so every Linux backend shares one mapping. The
/// optional logical key is adapter-supplied because it depends on the active
/// host or Weld-owned XKB keymap. Text input remains deferred with that keymap
/// work, and nested input currently drops unidentifiable Winit scancodes.
#[derive(Clone, Debug, PartialEq)]
pub struct RawSeatEvent {
    pub event: RawSeatEventKind,
    pub time: u32,
}

impl RawSeatEvent {
    pub const fn new(event: RawSeatEventKind, time: u32) -> Self {
        Self { event, time }
    }

    /// Pointer-presence change carried by this transition.
    pub(crate) const fn pointer_update(&self) -> Option<RawPointerUpdate> {
        match &self.event {
            RawSeatEventKind::PointerMotion { position } => {
                Some(RawPointerUpdate::Position(*position))
            }
            RawSeatEventKind::PointerLeft { .. } | RawSeatEventKind::HostFocusLost => {
                Some(RawPointerUpdate::Clear)
            }
            RawSeatEventKind::PointerButton { .. }
            | RawSeatEventKind::PointerAxis { .. }
            | RawSeatEventKind::PointerGesture { .. }
            | RawSeatEventKind::Keyboard { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum RawPointerUpdate {
    Position(InputPosition),
    Clear,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RawSeatEventKind {
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
    PointerGesture {
        gesture: PointerGesture,
    },
    Keyboard {
        keycode: LinuxKeycode,
        logical_key: Option<Key>,
        state: ButtonState,
    },
    HostFocusLost,
}
