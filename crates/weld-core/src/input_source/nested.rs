//! Winit source for the backend-neutral raw input stream.

use std::collections::{VecDeque, vec_deque::Drain};

use tracing::trace;
use winit::{
    dpi::PhysicalPosition,
    event::{ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent},
    platform::scancode::PhysicalKeyExtScancode,
};

use crate::input::{
    ButtonState, InputPosition, LinuxButtonCode, LinuxKeycode, RawScrollFrame, RawScrollPhase,
    RawScrollSource, RawSeatEvent, RawSeatEventKind,
};

#[derive(Default)]
pub struct NestedAdapter {
    events: VecDeque<RawSeatEvent>,
    pointer_position: Option<InputPosition>,
    active_scroll_axes: ActiveScrollAxes,
}

impl NestedAdapter {
    pub fn has_pending(&self) -> bool {
        !self.events.is_empty()
    }

    pub fn drain(&mut self) -> Drain<'_, RawSeatEvent> {
        self.events.drain(..)
    }

    pub fn handle_window_event(&mut self, event: WindowEvent, scale_factor: f64, time: u32) {
        match event {
            WindowEvent::Focused(false) => {
                self.pointer_position = None;
                self.cancel_active_scroll(time);
                self.events
                    .push_back(RawSeatEvent::new(RawSeatEventKind::HostFocusLost, time));
            }
            // Click-to-focus is deliberate: regaining host focus does not restore
            // the previously focused client automatically.
            WindowEvent::Focused(true) => {}
            WindowEvent::CursorMoved { position, .. } => {
                let position = logical_input_position(position, scale_factor);
                self.pointer_position = Some(position);
                self.events.push_back(RawSeatEvent::new(
                    RawSeatEventKind::PointerMotion { position },
                    time,
                ));
            }
            WindowEvent::CursorLeft { .. } => {
                let position = self.pointer_position.take().unwrap_or_default();
                self.cancel_active_scroll(time);
                self.events.push_back(RawSeatEvent::new(
                    RawSeatEventKind::PointerLeft { position },
                    time,
                ));
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = linux_button_code(button) {
                    self.events.push_back(RawSeatEvent::new(
                        RawSeatEventKind::PointerButton {
                            position: self.pointer_position,
                            button,
                            state: button_state(state),
                        },
                        time,
                    ));
                }
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                trace!(?delta, ?phase, "received nested host scroll");
                let axis = nested_axis(delta, phase, scale_factor, &mut self.active_scroll_axes);
                self.events.push_back(RawSeatEvent::new(
                    RawSeatEventKind::PointerAxis {
                        position: self.pointer_position,
                        axis,
                    },
                    time,
                ));
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } if !is_synthetic && !event.repeat => {
                if let Some(keycode) = event.physical_key.to_scancode() {
                    self.events.push_back(RawSeatEvent::new(
                        RawSeatEventKind::Keyboard {
                            keycode: LinuxKeycode(keycode),
                            logical_key: Some(event.logical_key.clone()),
                            state: button_state(event.state),
                        },
                        time,
                    ));
                }
            }
            _ => {}
        }
    }

    fn cancel_active_scroll(&mut self, time: u32) {
        if !self.active_scroll_axes.horizontal && !self.active_scroll_axes.vertical {
            return;
        }
        self.events.push_back(RawSeatEvent::new(
            RawSeatEventKind::PointerAxis {
                position: self.pointer_position,
                axis: RawScrollFrame {
                    source: RawScrollSource::Finger,
                    phase: RawScrollPhase::Cancelled,
                    horizontal: 0.0,
                    vertical: 0.0,
                    horizontal_v120: None,
                    vertical_v120: None,
                    horizontal_stop: self.active_scroll_axes.horizontal,
                    vertical_stop: self.active_scroll_axes.vertical,
                },
            },
            time,
        ));
        self.active_scroll_axes = ActiveScrollAxes::default();
    }
}

const fn button_state(state: ElementState) -> ButtonState {
    match state {
        ElementState::Pressed => ButtonState::Pressed,
        ElementState::Released => ButtonState::Released,
    }
}

#[derive(Default)]
struct ActiveScrollAxes {
    horizontal: bool,
    vertical: bool,
}

const fn linux_button_code(button: MouseButton) -> Option<LinuxButtonCode> {
    match button {
        MouseButton::Left => Some(LinuxButtonCode(0x110)),
        MouseButton::Right => Some(LinuxButtonCode(0x111)),
        MouseButton::Middle => Some(LinuxButtonCode(0x112)),
        MouseButton::Forward => Some(LinuxButtonCode(0x114)),
        MouseButton::Back => Some(LinuxButtonCode(0x113)),
        MouseButton::Other(_) => None,
    }
}

fn nested_axis(
    delta: MouseScrollDelta,
    phase: TouchPhase,
    scale_factor: f64,
    active: &mut ActiveScrollAxes,
) -> RawScrollFrame {
    match delta {
        MouseScrollDelta::LineDelta(horizontal, vertical) => RawScrollFrame {
            source: RawScrollSource::Wheel,
            phase: RawScrollPhase::Moved,
            horizontal: -f64::from(horizontal) * 15.0,
            vertical: -f64::from(vertical) * 15.0,
            horizontal_v120: Some((-horizontal * 120.0) as i32),
            vertical_v120: Some((-vertical * 120.0) as i32),
            horizontal_stop: false,
            vertical_stop: false,
        },
        MouseScrollDelta::PixelDelta(delta) => {
            let horizontal = -delta.x / scale_factor;
            let vertical = -delta.y / scale_factor;
            let phase = raw_scroll_phase(phase);
            let ending = matches!(phase, RawScrollPhase::Ended | RawScrollPhase::Cancelled);
            let horizontal_stop = ending && active.horizontal;
            let vertical_stop = ending && active.vertical;
            if ending {
                *active = ActiveScrollAxes::default();
            } else {
                active.horizontal |= horizontal != 0.0;
                active.vertical |= vertical != 0.0;
            }
            RawScrollFrame {
                source: RawScrollSource::Finger,
                phase,
                horizontal,
                vertical,
                horizontal_v120: None,
                vertical_v120: None,
                horizontal_stop,
                vertical_stop,
            }
        }
    }
}

const fn raw_scroll_phase(phase: TouchPhase) -> RawScrollPhase {
    match phase {
        TouchPhase::Started => RawScrollPhase::Started,
        TouchPhase::Moved => RawScrollPhase::Moved,
        TouchPhase::Ended => RawScrollPhase::Ended,
        TouchPhase::Cancelled => RawScrollPhase::Cancelled,
    }
}

fn logical_input_position(position: PhysicalPosition<f64>, scale_factor: f64) -> InputPosition {
    InputPosition::new(position.x / scale_factor, position.y / scale_factor)
}

#[cfg(test)]
mod tests {
    use winit::{
        dpi::PhysicalPosition,
        event::{MouseButton, MouseScrollDelta, TouchPhase},
    };

    use super::{ActiveScrollAxes, linux_button_code, logical_input_position, nested_axis};
    use crate::input::{
        InputPosition, LinuxButtonCode, RawScrollFrame, RawScrollPhase, RawScrollSource,
    };

    #[test]
    fn mouse_buttons_map_to_canonical_linux_codes() {
        let cases = [
            (MouseButton::Left, Some(LinuxButtonCode(0x110))),
            (MouseButton::Right, Some(LinuxButtonCode(0x111))),
            (MouseButton::Middle, Some(LinuxButtonCode(0x112))),
            (MouseButton::Back, Some(LinuxButtonCode(0x113))),
            (MouseButton::Forward, Some(LinuxButtonCode(0x114))),
            (MouseButton::Other(9), None),
        ];

        for (button, expected) in cases {
            assert_eq!(linux_button_code(button), expected);
        }
    }

    #[test]
    fn nested_scroll_converts_to_wayland_axis_direction() {
        assert_eq!(
            nested_axis(
                MouseScrollDelta::LineDelta(2.0, -3.0),
                TouchPhase::Cancelled,
                1.25,
                &mut ActiveScrollAxes::default(),
            ),
            RawScrollFrame {
                source: RawScrollSource::Wheel,
                phase: RawScrollPhase::Moved,
                horizontal: -30.0,
                vertical: 45.0,
                horizontal_v120: Some(-240),
                vertical_v120: Some(360),
                horizontal_stop: false,
                vertical_stop: false,
            }
        );
        let mut active = ActiveScrollAxes::default();
        assert_eq!(
            nested_axis(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(5.0, -2.5)),
                TouchPhase::Started,
                1.25,
                &mut active,
            ),
            RawScrollFrame {
                source: RawScrollSource::Finger,
                phase: RawScrollPhase::Started,
                horizontal: -4.0,
                vertical: 2.0,
                horizontal_v120: None,
                vertical_v120: None,
                horizontal_stop: false,
                vertical_stop: false,
            }
        );
        assert_eq!(
            nested_axis(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.0)),
                TouchPhase::Ended,
                1.25,
                &mut active,
            ),
            RawScrollFrame {
                source: RawScrollSource::Finger,
                phase: RawScrollPhase::Ended,
                horizontal: 0.0,
                vertical: 0.0,
                horizontal_v120: None,
                vertical_v120: None,
                horizontal_stop: true,
                vertical_stop: true,
            }
        );
    }

    #[test]
    fn host_physical_pointer_positions_become_compositor_logical() {
        assert_eq!(
            logical_input_position(PhysicalPosition::new(100.0, 50.0), 1.25),
            InputPosition::new(80.0, 40.0)
        );
    }
}
