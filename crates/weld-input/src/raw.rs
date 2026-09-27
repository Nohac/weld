//! Backend seat input before adapter-addressed client routing.

use winit::keyboard::Key;

pub use weld_client::{
    ButtonState, InputDelta, InputPosition, KeyboardKeyState, LinuxButtonCode, LinuxKeycode,
    PointerGesture, PointerGestureKind, RawScrollFrame, RawScrollPhase, RawScrollSource,
    TouchpadHold, TouchpadPinch, TouchpadSwipe,
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
    /// Effective Weld keymap modifiers after this transition.
    pub modifiers: Option<SeatModifiers>,
}

impl RawSeatEvent {
    pub const fn new(event: RawSeatEventKind, time: u32) -> Self {
        Self {
            event,
            time,
            modifiers: None,
        }
    }

    pub const fn with_modifiers(mut self, modifiers: SeatModifiers) -> Self {
        self.modifiers = Some(modifiers);
        self
    }

    pub fn into_runtime(self) -> weld_client::RuntimeInputEvent {
        let event = match self.event {
            RawSeatEventKind::PointerMotion { position } => {
                weld_client::InputEventKind::PointerMotion { position }
            }
            RawSeatEventKind::PointerLeft { position } => {
                weld_client::InputEventKind::PointerLeft { position }
            }
            RawSeatEventKind::PointerButton {
                position,
                button,
                state,
            } => weld_client::InputEventKind::PointerButton {
                position,
                button,
                state,
            },
            RawSeatEventKind::PointerAxis { position, axis } => {
                weld_client::InputEventKind::PointerAxis { position, axis }
            }
            RawSeatEventKind::PointerGesture { gesture } => {
                weld_client::InputEventKind::PointerGesture { gesture }
            }
            RawSeatEventKind::Keyboard { keycode, state, .. } => {
                weld_client::InputEventKind::Keyboard { keycode, state }
            }
            RawSeatEventKind::HostFocusLost => {
                return weld_client::RuntimeInputEvent::new(
                    weld_client::RuntimeInputEventKind::HostFocusLost,
                    self.time,
                );
            }
        };
        weld_client::RuntimeInputEvent::new(
            weld_client::RuntimeInputEventKind::Input(event),
            self.time,
        )
    }
}

/// Effective modifiers used when matching shell keyboard and pointer chords.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SeatModifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
}

impl SeatModifiers {
    /// Default physical modifier positions for synthetic sources that have not
    /// passed through a configured native keyboard mapper.
    pub fn from_pressed_keys(pressed: &std::collections::HashSet<LinuxKeycode>) -> Self {
        let any = |codes: &[u32]| {
            codes
                .iter()
                .any(|code| pressed.contains(&LinuxKeycode(*code)))
        };
        Self {
            control: any(&[29, 97]),
            alt: any(&[56, 100]),
            shift: any(&[42, 54]),
            super_key: any(&[125, 126]),
        }
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
        state: KeyboardKeyState,
    },
    HostFocusLost,
}
