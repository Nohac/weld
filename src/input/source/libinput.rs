//! Libinput source for the backend-neutral raw input stream.

use bevy::input::ButtonState as BevyButtonState;
use smithay::backend::{
    input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputEvent, KeyState,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    libinput::LibinputInputBackend,
};

use crate::input::raw::{
    InputPosition, LinuxButtonCode, LinuxKeycode, RawScrollFrame, RawScrollPhase, RawScrollSource,
    RawSeatEvent, RawSeatEventKind,
};

pub(crate) struct LibinputAdapter {
    logical_width: f64,
    logical_height: f64,
    pointer: InputPosition,
    finger_axes: ActiveScrollAxes,
}

#[derive(Default)]
struct ActiveScrollAxes {
    horizontal: bool,
    vertical: bool,
}

impl LibinputAdapter {
    pub(crate) fn new(logical_width: f64, logical_height: f64) -> Self {
        Self {
            logical_width,
            logical_height,
            pointer: InputPosition::new(logical_width / 2.0, logical_height / 2.0),
            finger_axes: ActiveScrollAxes::default(),
        }
    }

    pub(crate) fn initial_event(&self) -> RawSeatEvent {
        RawSeatEvent::new(
            RawSeatEventKind::PointerMotion {
                position: self.pointer,
            },
            0,
        )
    }

    pub(crate) fn convert(
        &mut self,
        event: InputEvent<LibinputInputBackend>,
    ) -> Option<RawSeatEvent> {
        match event {
            InputEvent::Keyboard { event, .. } => {
                let raw: u32 = event.key_code().into();
                raw.checked_sub(8).map(|keycode| {
                    RawSeatEvent::new(
                        RawSeatEventKind::Keyboard {
                            keycode: LinuxKeycode(keycode),
                            logical_key: None,
                            state: match event.state() {
                                KeyState::Pressed => BevyButtonState::Pressed,
                                KeyState::Released => BevyButtonState::Released,
                            },
                        },
                        event.time_msec(),
                    )
                })
            }
            InputEvent::PointerMotion { event, .. } => {
                self.pointer = self.clamp(InputPosition::new(
                    self.pointer.x + event.delta_x(),
                    self.pointer.y + event.delta_y(),
                ));
                Some(RawSeatEvent::new(
                    RawSeatEventKind::PointerMotion {
                        position: self.pointer,
                    },
                    event.time_msec(),
                ))
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                self.pointer = self.clamp(InputPosition::new(
                    event.x_transformed(self.logical_width as i32),
                    event.y_transformed(self.logical_height as i32),
                ));
                Some(RawSeatEvent::new(
                    RawSeatEventKind::PointerMotion {
                        position: self.pointer,
                    },
                    event.time_msec(),
                ))
            }
            InputEvent::PointerButton { event, .. } => Some(RawSeatEvent::new(
                RawSeatEventKind::PointerButton {
                    position: Some(self.pointer),
                    button: LinuxButtonCode(event.button_code()),
                    state: match event.state() {
                        ButtonState::Pressed => BevyButtonState::Pressed,
                        ButtonState::Released => BevyButtonState::Released,
                    },
                },
                event.time_msec(),
            )),
            InputEvent::PointerAxis { event, .. } => {
                let source = match event.source() {
                    AxisSource::Wheel | AxisSource::WheelTilt => RawScrollSource::Wheel,
                    AxisSource::Finger => RawScrollSource::Finger,
                    AxisSource::Continuous => RawScrollSource::Continuous,
                };
                Some(RawSeatEvent::new(
                    RawSeatEventKind::PointerAxis {
                        position: Some(self.pointer),
                        axis: self.scroll_frame(
                            source,
                            event.amount(Axis::Horizontal),
                            event.amount(Axis::Vertical),
                            event
                                .amount_v120(Axis::Horizontal)
                                .map(|value| value as i32),
                            event.amount_v120(Axis::Vertical).map(|value| value as i32),
                        ),
                    },
                    event.time_msec(),
                ))
            }
            _ => None,
        }
    }

    fn clamp(&self, position: InputPosition) -> InputPosition {
        InputPosition::new(
            position.x.clamp(0.0, (self.logical_width - 1.0).max(0.0)),
            position.y.clamp(0.0, (self.logical_height - 1.0).max(0.0)),
        )
    }

    fn scroll_frame(
        &mut self,
        source: RawScrollSource,
        horizontal: Option<f64>,
        vertical: Option<f64>,
        horizontal_v120: Option<i32>,
        vertical_v120: Option<i32>,
    ) -> RawScrollFrame {
        let was_active = self.finger_axes.horizontal || self.finger_axes.vertical;
        let horizontal_stop = source == RawScrollSource::Finger && horizontal == Some(0.0);
        let vertical_stop = source == RawScrollSource::Finger && vertical == Some(0.0);
        if source == RawScrollSource::Finger {
            if horizontal.is_some_and(|amount| amount != 0.0) {
                self.finger_axes.horizontal = true;
            } else if horizontal_stop {
                self.finger_axes.horizontal = false;
            }
            if vertical.is_some_and(|amount| amount != 0.0) {
                self.finger_axes.vertical = true;
            } else if vertical_stop {
                self.finger_axes.vertical = false;
            }
        }
        let is_active = self.finger_axes.horizontal || self.finger_axes.vertical;
        let phase = if source != RawScrollSource::Finger {
            RawScrollPhase::Moved
        } else if !was_active && is_active {
            RawScrollPhase::Started
        } else if was_active && !is_active {
            RawScrollPhase::Ended
        } else {
            RawScrollPhase::Moved
        };
        RawScrollFrame {
            source,
            phase,
            horizontal: horizontal.unwrap_or_default(),
            vertical: vertical.unwrap_or_default(),
            horizontal_v120,
            vertical_v120,
            horizontal_stop,
            vertical_stop,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LibinputAdapter;
    use crate::input::raw::{
        InputPosition, RawScrollPhase, RawScrollSource, RawSeatEvent, RawSeatEventKind,
    };

    #[test]
    fn standalone_pointer_starts_centered_and_stays_inside_the_output() {
        let adapter = LibinputAdapter::new(1920.0, 1080.0);
        assert_eq!(
            adapter.initial_event(),
            RawSeatEvent::new(
                RawSeatEventKind::PointerMotion {
                    position: InputPosition::new(960.0, 540.0),
                },
                0,
            )
        );
        assert_eq!(
            adapter.clamp(InputPosition::new(-20.0, 5000.0)),
            InputPosition::new(0.0, 1079.0)
        );
    }

    #[test]
    fn standalone_finger_scroll_stops_only_the_reported_axis() {
        let mut adapter = LibinputAdapter::new(1920.0, 1080.0);
        let started =
            adapter.scroll_frame(RawScrollSource::Finger, Some(4.0), Some(6.0), None, None);
        assert_eq!(started.phase, RawScrollPhase::Started);

        let horizontal_stop =
            adapter.scroll_frame(RawScrollSource::Finger, Some(0.0), None, None, None);
        assert!(horizontal_stop.horizontal_stop);
        assert!(!horizontal_stop.vertical_stop);
        assert_eq!(horizontal_stop.phase, RawScrollPhase::Moved);

        let vertical_stop =
            adapter.scroll_frame(RawScrollSource::Finger, None, Some(0.0), None, None);
        assert!(!vertical_stop.horizontal_stop);
        assert!(vertical_stop.vertical_stop);
        assert_eq!(vertical_stop.phase, RawScrollPhase::Ended);
    }
}
