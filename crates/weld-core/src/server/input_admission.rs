//! Admission of complete input sequences across controller arbitration.

use std::collections::HashMap;
use weld_client::{
    ButtonState, ClientInputEvent, ClientSurfaceId, InputEventKind, KeyboardKeyState,
    RawScrollPhase, RawScrollSource,
};

#[derive(Default)]
pub(super) struct InputAdmission {
    keys: HashMap<u32, (ClientSurfaceId, bool)>,
    buttons: HashMap<u32, (ClientSurfaceId, bool)>,
    gesture: Option<bool>,
    scroll: Option<bool>,
}

impl InputAdmission {
    pub fn allows(&self, event: &InputEventKind) -> bool {
        match event {
            InputEventKind::Keyboard { keycode, state } => match state {
                KeyboardKeyState::Pressed => !self.keys.contains_key(&keycode.0),
                _ => self
                    .keys
                    .get(&keycode.0)
                    .is_some_and(|(_, accepted)| *accepted),
            },
            InputEventKind::PointerButton { button, state, .. } => match state {
                ButtonState::Pressed => !self.buttons.contains_key(&button.0),
                ButtonState::Released => self
                    .buttons
                    .get(&button.0)
                    .is_some_and(|(_, accepted)| *accepted),
            },
            InputEventKind::PointerGesture { gesture } => {
                gesture.is_begin() || self.gesture == Some(true)
            }
            InputEventKind::PointerAxis { axis, .. } if axis.source == RawScrollSource::Finger => {
                axis.phase == RawScrollPhase::Started
                    || self.scroll == Some(true)
                    || (axis.phase == RawScrollPhase::Moved && self.scroll.is_none())
            }
            _ => true,
        }
    }

    pub fn observe(&mut self, input: &ClientInputEvent, accepted: bool) {
        let surface = input.target.surface();
        match &input.event {
            InputEventKind::Keyboard { keycode, state } => match state {
                KeyboardKeyState::Pressed => {
                    self.keys.entry(keycode.0).or_insert((surface, accepted));
                }
                KeyboardKeyState::Released => {
                    self.keys.remove(&keycode.0);
                }
                KeyboardKeyState::Repeated => {}
            },
            InputEventKind::PointerButton { button, state, .. } => match state {
                ButtonState::Pressed => {
                    self.buttons.entry(button.0).or_insert((surface, accepted));
                }
                ButtonState::Released => {
                    self.buttons.remove(&button.0);
                }
            },
            InputEventKind::PointerGesture { gesture } => {
                if gesture.is_begin() {
                    self.gesture = Some(accepted);
                } else if gesture.is_end() {
                    self.gesture = None;
                }
            }
            InputEventKind::PointerAxis { axis, .. } if axis.source == RawScrollSource::Finger => {
                if matches!(
                    axis.phase,
                    RawScrollPhase::Ended | RawScrollPhase::Cancelled
                ) {
                    self.scroll = None;
                } else {
                    self.scroll.get_or_insert(accepted);
                }
            }
            _ => {}
        }
    }

    pub fn retire_surface(&mut self, surface: ClientSurfaceId) {
        self.keys.retain(|_, (owner, _)| *owner != surface);
        self.buttons.retain(|_, (owner, _)| *owner != surface);
    }

    pub fn clear(&mut self) {
        self.keys.clear();
        self.buttons.clear();
        self.gesture = None;
        self.scroll = None;
    }
}

/// Hover and trailing events preserve another controller's effective focus.
/// A wheel step is an atomic interaction; finger scroll and gestures have a begin.
pub(super) fn starts_interaction(event: &InputEventKind) -> bool {
    match event {
        InputEventKind::Keyboard {
            state: KeyboardKeyState::Pressed,
            ..
        }
        | InputEventKind::PointerButton {
            state: ButtonState::Pressed,
            ..
        } => true,
        InputEventKind::PointerGesture { gesture } => gesture.is_begin(),
        InputEventKind::PointerAxis { axis, .. } => {
            axis.phase == RawScrollPhase::Started
                || (axis.source != RawScrollSource::Finger
                    && (axis.horizontal != 0.0
                        || axis.vertical != 0.0
                        || axis.horizontal_v120.is_some_and(|delta| delta != 0)
                        || axis.vertical_v120.is_some_and(|delta| delta != 0)))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_client::{
        ClientId, ClientInputTarget, ClientSourceId, InputPosition, LinuxKeycode, PointerGesture,
        TouchpadHold,
    };

    fn key(surface: ClientSurfaceId, state: KeyboardKeyState) -> ClientInputEvent {
        ClientInputEvent {
            target: ClientInputTarget::Keyboard { surface },
            host_position: None,
            event: InputEventKind::Keyboard {
                keycode: LinuxKeycode(30),
                state,
            },
            time: 1,
        }
    }

    #[test]
    fn rejected_sequence_remains_rejected_until_release_and_retirement_removes_it() {
        let surface = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 1), 1);
        let mut admission = InputAdmission::default();
        let press = key(surface, KeyboardKeyState::Pressed);
        assert!(admission.allows(&press.event));
        admission.observe(&press, false);
        let release = key(surface, KeyboardKeyState::Released);
        assert!(!admission.allows(&release.event));
        assert!(!admission.allows(&key(surface, KeyboardKeyState::Repeated).event));
        admission.observe(&release, false);
        assert!(admission.allows(&press.event));
        admission.observe(&press, true);
        assert!(admission.allows(&release.event));
        admission.retire_surface(surface);
        assert!(!admission.allows(&release.event));
        assert!(admission.allows(&press.event));
    }

    #[test]
    fn hover_and_sequence_tails_cannot_take_over_another_controller() {
        assert!(!starts_interaction(&InputEventKind::PointerMotion {
            position: InputPosition::new(0.0, 0.0),
            relative: None
        }));
        assert!(!starts_interaction(&InputEventKind::Keyboard {
            keycode: LinuxKeycode(30),
            state: KeyboardKeyState::Released
        }));
        assert!(starts_interaction(&InputEventKind::Keyboard {
            keycode: LinuxKeycode(30),
            state: KeyboardKeyState::Pressed
        }));
        assert!(starts_interaction(&InputEventKind::PointerGesture {
            gesture: PointerGesture::Hold(TouchpadHold::Begin { fingers: 2 })
        }));
        assert!(!starts_interaction(&InputEventKind::PointerGesture {
            gesture: PointerGesture::Hold(TouchpadHold::End { cancelled: false })
        }));
    }
}
