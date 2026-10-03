//! Backend-neutral input contracts and host input sources.

#[path = "input_keyboard.rs"]
mod keyboard;
#[path = "input_source/mod.rs"]
pub mod source;

pub(crate) use keyboard::KeyboardRepeatTracker;
pub use weld_input::{
    ButtonState, InputDelta, InputPosition, KeyboardKeyState, KeyboardKeymap, KeyboardMapper,
    KeyboardRepeatMode, KeyboardSettings, KeymapConfig, LegacyKeyRepeat, LinuxButtonCode,
    LinuxKeycode, PointerGesture, PointerGestureKind, RawScrollFrame, RawScrollPhase,
    RawScrollSource, RawSeatEvent, RawSeatEventKind, RelativeMotion, SeatModifiers, TouchpadHold,
    TouchpadPinch, TouchpadSwipe,
};

use crate::surface::{SurfaceId, SurfaceLayerId};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SurfaceHit {
    pub surface: SurfaceId,
    pub layer: SurfaceLayerId,
    pub local_position: InputPosition,
}
