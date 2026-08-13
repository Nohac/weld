//! Libinput source for the backend-neutral raw input stream.

use smithay::backend::{
    input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputEvent, KeyState,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    libinput::LibinputInputBackend,
};

use crate::input::{
    ButtonState as WeldButtonState, InputPosition, LinuxButtonCode, LinuxKeycode, RawScrollFrame,
    RawScrollPhase, RawScrollSource, RawSeatEvent, RawSeatEventKind,
};

pub struct LibinputAdapter {
    logical_width: f64,
    logical_height: f64,
    pointer: InputPosition,
    finger_axes: ActiveScrollAxes,
    last_event_time_msec: u32,
}

#[derive(Default)]
struct ActiveScrollAxes {
    horizontal: bool,
    vertical: bool,
}

impl LibinputAdapter {
    pub fn new(logical_width: f64, logical_height: f64) -> Self {
        Self {
            logical_width,
            logical_height,
            pointer: InputPosition::new(logical_width / 2.0, logical_height / 2.0),
            finger_axes: ActiveScrollAxes::default(),
            last_event_time_msec: 0,
        }
    }

    pub fn initial_event(&self) -> RawSeatEvent {
        RawSeatEvent::new(
            RawSeatEventKind::PointerMotion {
                position: self.pointer,
            },
            0,
        )
    }

    pub fn convert(&mut self, event: InputEvent<LibinputInputBackend>) -> Option<RawSeatEvent> {
        match event {
            InputEvent::Keyboard { event, .. } => {
                self.last_event_time_msec = event.time_msec();
                let raw: u32 = event.key_code().into();
                raw.checked_sub(8).map(|keycode| {
                    RawSeatEvent::new(
                        RawSeatEventKind::Keyboard {
                            keycode: LinuxKeycode(keycode),
                            logical_key: None,
                            state: match event.state() {
                                KeyState::Pressed => WeldButtonState::Pressed,
                                KeyState::Released => WeldButtonState::Released,
                            },
                        },
                        event.time_msec(),
                    )
                })
            }
            InputEvent::PointerMotion { event, .. } => {
                self.last_event_time_msec = event.time_msec();
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
                self.last_event_time_msec = event.time_msec();
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
            InputEvent::PointerButton { event, .. } => {
                self.last_event_time_msec = event.time_msec();
                Some(RawSeatEvent::new(
                    RawSeatEventKind::PointerButton {
                        position: Some(self.pointer),
                        button: LinuxButtonCode(event.button_code()),
                        state: match event.state() {
                            ButtonState::Pressed => WeldButtonState::Pressed,
                            ButtonState::Released => WeldButtonState::Released,
                        },
                    },
                    event.time_msec(),
                ))
            }
            InputEvent::PointerAxis { event, .. } => {
                self.last_event_time_msec = event.time_msec();
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

    /// Updates logical bounds after a scale-only output change.
    ///
    /// The physical mode is unchanged, so preserving the pointer's normalized
    /// location also preserves its physical display location. A motion event is
    /// always returned for changed bounds so compositor focus is recomputed
    /// even when rounding leaves the logical coordinates unchanged.
    pub fn update_output_bounds(
        &mut self,
        logical_width: f64,
        logical_height: f64,
    ) -> Option<RawSeatEvent> {
        if self.logical_width == logical_width && self.logical_height == logical_height {
            return None;
        }
        let position = InputPosition::new(
            self.pointer.x * logical_width / self.logical_width,
            self.pointer.y * logical_height / self.logical_height,
        );
        self.logical_width = logical_width;
        self.logical_height = logical_height;
        self.pointer = self.clamp(position);
        Some(RawSeatEvent::new(
            RawSeatEventKind::PointerMotion {
                position: self.pointer,
            },
            self.last_event_time_msec,
        ))
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
    use crate::input::{
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
    fn scale_only_bounds_change_preserves_physical_pointer_location_and_time() {
        let mut adapter = LibinputAdapter::new(1920.0, 1080.0);
        adapter.pointer = InputPosition::new(1440.0, 810.0);
        adapter.last_event_time_msec = 42;

        assert_eq!(
            adapter.update_output_bounds(1280.0, 720.0),
            Some(RawSeatEvent::new(
                RawSeatEventKind::PointerMotion {
                    position: InputPosition::new(960.0, 540.0),
                },
                42,
            ))
        );
        assert_eq!(adapter.update_output_bounds(1280.0, 720.0), None);
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
