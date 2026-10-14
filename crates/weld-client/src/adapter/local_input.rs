//! Surface-scoped local handoff while physical input sequences are still active.

use super::{ClientInputRetirement, ClientRuntime, PointerCapture};
use crate::{
    ButtonState, ClientInputEvent, ClientInputTarget, ClientRequest, InputEventKind,
    KeyboardKeyState, LinuxButtonCode, RawScrollFrame, RuntimeInputEventKind,
};

impl ClientRuntime {
    /// Release this surface's local sequences and retain suppression until their
    /// physical release or a fresh press starts a new sequence.
    pub fn retire_local_input(&mut self, retirement: ClientInputRetirement) {
        let Ok(surface) = self.resolve_alias(retirement.surface) else {
            return;
        };
        let time = retirement.time;
        self.cancel_surface_touch(surface);
        let keys: Vec<_> = self
            .keyboard_captures
            .iter()
            .filter_map(|(key, capture)| {
                self.surface_resolves_to(capture.route.surface, surface)
                    .then_some(*key)
            })
            .collect();
        for key in keys {
            self.keyboard_captures.remove(&key);
            self.retired_local_keys.insert(key);
            self.dispatch_to(
                surface.source(),
                ClientInputEvent {
                    target: ClientInputTarget::Keyboard { surface },
                    host_position: None,
                    event: InputEventKind::Keyboard {
                        keycode: key,
                        state: KeyboardKeyState::Released,
                    },
                    time,
                },
            );
        }
        if let Some(capture) = self.gesture_capture
            && self.route_resolves_to(capture.route, surface)
        {
            self.gesture_capture = None;
            self.dispatch_pointer_to(
                capture.route,
                InputEventKind::PointerGesture {
                    gesture: capture.kind.cancelled(),
                },
                None,
                time,
            );
        }
        if let Some(capture) = self.finger_scroll_capture
            && self.route_resolves_to(capture.route, surface)
        {
            self.finger_scroll_capture = None;
            self.dispatch_pointer_to(
                capture.route,
                InputEventKind::PointerAxis {
                    position: self.pointer_position,
                    axis: RawScrollFrame::cancelled_finger(
                        capture.horizontal_active,
                        capture.vertical_active,
                    ),
                },
                self.pointer_position,
                time,
            );
        }
        let pointer = self.captured_or_current_pointer_route().ok().flatten();
        if self.route_resolves_to(pointer, surface) {
            let buttons: Vec<_> = self
                .pressed_buttons
                .0
                .iter()
                .enumerate()
                .flat_map(|(word_index, word)| {
                    (0..u64::BITS)
                        .filter(move |bit| word & (1_u64 << bit) != 0)
                        .map(move |bit| LinuxButtonCode(word_index as u32 * u64::BITS + bit))
                })
                .collect();
            for button in buttons {
                self.retired_local_buttons.insert(button);
                self.dispatch_pointer_to(
                    pointer,
                    InputEventKind::PointerButton {
                        position: self.pointer_position,
                        button,
                        state: ButtonState::Released,
                    },
                    self.pointer_position,
                    time,
                );
            }
            self.pressed_buttons.clear();
            self.pointer_capture = PointerCapture::Idle;
            self.dispatch_pointer_to(
                pointer,
                InputEventKind::PointerLeft {
                    position: self.pointer_position.unwrap_or_default(),
                },
                self.pointer_position,
                time,
            );
        }
        if self.route_resolves_to(self.pointer_route, surface) {
            self.pointer_route = None;
        }
        if self
            .pending_pointer_route
            .is_some_and(|route| self.route_resolves_to(route, surface))
        {
            self.pending_pointer_route = Some(None);
        }
        if matches!(self.pointer_capture, PointerCapture::Idle)
            && let Some(pending) = self.pending_pointer_route.take()
        {
            self.pointer_route = pending;
        }
        if self
            .keyboard_route
            .is_some_and(|route| self.surface_resolves_to(route.surface, surface))
        {
            self.apply_request(ClientRequest::ClearFocus);
        }
        if let Some(adapter) = self.adapters.get_mut(&surface.source()) {
            adapter.driver.retire_local_input(surface, time);
        }
    }

    pub(super) fn consume_retired_local_edge(&mut self, event: &RuntimeInputEventKind) -> bool {
        match event {
            RuntimeInputEventKind::Input(InputEventKind::Keyboard { keycode, state })
                if self.retired_local_keys.contains(keycode) =>
            {
                if *state != KeyboardKeyState::Repeated {
                    self.retired_local_keys.remove(keycode);
                }
                *state != KeyboardKeyState::Pressed
            }
            RuntimeInputEventKind::Input(InputEventKind::PointerButton {
                button, state, ..
            }) if self.retired_local_buttons.contains(button) => {
                self.retired_local_buttons.remove(button);
                *state == ButtonState::Released
            }
            _ => false,
        }
    }
}
