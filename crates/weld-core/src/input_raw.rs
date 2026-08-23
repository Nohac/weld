//! Backend seat input before adapter-addressed client routing.

use winit::keyboard::Key;

pub use weld_client::{
    ButtonState, InputDelta, InputPosition, LinuxButtonCode, LinuxKeycode, PointerGesture,
    PointerGestureKind, RawScrollFrame, RawScrollPhase, RawScrollSource, TouchpadHold,
    TouchpadPinch, TouchpadSwipe,
};

/// One ordered input transition from a nested or standalone seat backend.
///
/// The optional logical key remains host-side because shortcut projection uses
/// the active host or Weld-owned keymap. Client adapters receive the canonical
/// Linux code through `weld-client` after compositor shortcuts decline it.
#[derive(Clone, Debug, PartialEq)]
pub struct RawSeatEvent {
    pub event: RawSeatEventKind,
    pub time: u32,
}

impl RawSeatEvent {
    pub const fn new(event: RawSeatEventKind, time: u32) -> Self {
        Self { event, time }
    }
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
