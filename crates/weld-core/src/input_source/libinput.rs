//! Libinput source for the backend-neutral raw input stream.

use smithay::backend::{
    input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, GestureBeginEvent,
        GestureEndEvent, GesturePinchUpdateEvent as _, GestureSwipeUpdateEvent as _, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    libinput::LibinputInputBackend,
};
use smithay::reexports::input::{ClickMethod, ClickfingerButtonMap, Device, TapButtonMap};
use tracing::{debug, trace, warn};

use crate::input::{
    ButtonState as WeldButtonState, InputDelta, InputPosition, LinuxButtonCode, LinuxKeycode,
    PointerGesture, PointerGestureKind, RawScrollFrame, RawScrollPhase, RawScrollSource,
    RawSeatEvent, RawSeatEventKind, TouchpadHold, TouchpadPinch, TouchpadSwipe,
};

pub struct LibinputAdapter {
    logical_width: f64,
    logical_height: f64,
    pointer: InputPosition,
    finger_axes: ActiveScrollAxes,
    active_gesture: ActiveGesture,
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
            active_gesture: ActiveGesture::default(),
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

    pub fn convert(
        &mut self,
        event: InputEvent<LibinputInputBackend>,
    ) -> [Option<RawSeatEvent>; 2] {
        let converted = match event {
            InputEvent::DeviceAdded { mut device } => {
                configure_pointer_device(&mut device);
                empty_batch()
            }
            InputEvent::Keyboard { event, .. } => {
                let raw: u32 = event.key_code().into();
                single_event(raw.checked_sub(8).map(|keycode| {
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
                }))
            }
            InputEvent::PointerMotion { event, .. } => {
                self.pointer = self.clamp(InputPosition::new(
                    self.pointer.x + event.delta_x(),
                    self.pointer.y + event.delta_y(),
                ));
                single_event(Some(RawSeatEvent::new(
                    RawSeatEventKind::PointerMotion {
                        position: self.pointer,
                    },
                    event.time_msec(),
                )))
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                self.pointer = self.clamp(InputPosition::new(
                    event.x_transformed(self.logical_width as i32),
                    event.y_transformed(self.logical_height as i32),
                ));
                single_event(Some(RawSeatEvent::new(
                    RawSeatEventKind::PointerMotion {
                        position: self.pointer,
                    },
                    event.time_msec(),
                )))
            }
            InputEvent::PointerButton { event, .. } => single_event(Some(RawSeatEvent::new(
                RawSeatEventKind::PointerButton {
                    position: Some(self.pointer),
                    button: LinuxButtonCode(event.button_code()),
                    state: match event.state() {
                        ButtonState::Pressed => WeldButtonState::Pressed,
                        ButtonState::Released => WeldButtonState::Released,
                    },
                },
                event.time_msec(),
            ))),
            InputEvent::PointerAxis { event, .. } => {
                let source = match event.source() {
                    AxisSource::Wheel | AxisSource::WheelTilt => RawScrollSource::Wheel,
                    AxisSource::Finger => RawScrollSource::Finger,
                    AxisSource::Continuous => RawScrollSource::Continuous,
                };
                single_event(Some(RawSeatEvent::new(
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
                )))
            }
            InputEvent::GestureSwipeBegin { event } => self.active_gesture.transition(
                PointerGesture::Swipe(TouchpadSwipe::Begin {
                    fingers: event.fingers(),
                }),
                event.time_msec(),
            ),
            InputEvent::GestureSwipeUpdate { event } => self.active_gesture.transition(
                PointerGesture::Swipe(TouchpadSwipe::Update {
                    delta: InputDelta::new(event.delta_x(), event.delta_y()),
                }),
                event.time_msec(),
            ),
            InputEvent::GestureSwipeEnd { event } => self.active_gesture.transition(
                PointerGesture::Swipe(TouchpadSwipe::End {
                    cancelled: event.cancelled(),
                }),
                event.time_msec(),
            ),
            InputEvent::GesturePinchBegin { event } => self.active_gesture.transition(
                PointerGesture::Pinch(TouchpadPinch::Begin {
                    fingers: event.fingers(),
                }),
                event.time_msec(),
            ),
            InputEvent::GesturePinchUpdate { event } => self.active_gesture.transition(
                PointerGesture::Pinch(TouchpadPinch::Update {
                    delta: InputDelta::new(event.delta_x(), event.delta_y()),
                    scale: event.scale(),
                    rotation: event.rotation(),
                }),
                event.time_msec(),
            ),
            InputEvent::GesturePinchEnd { event } => self.active_gesture.transition(
                PointerGesture::Pinch(TouchpadPinch::End {
                    cancelled: event.cancelled(),
                }),
                event.time_msec(),
            ),
            InputEvent::GestureHoldBegin { event } => self.active_gesture.transition(
                PointerGesture::Hold(TouchpadHold::Begin {
                    fingers: event.fingers(),
                }),
                event.time_msec(),
            ),
            InputEvent::GestureHoldEnd { event } => self.active_gesture.transition(
                PointerGesture::Hold(TouchpadHold::End {
                    cancelled: event.cancelled(),
                }),
                event.time_msec(),
            ),
            _ => empty_batch(),
        };
        if let Some(event) = converted.iter().flatten().last() {
            self.last_event_time_msec = event.time;
        }
        converted
    }

    /// Cancels active libinput streams before the logical seat loses focus.
    ///
    /// The gesture end precedes the finger-scroll cancellation. The caller
    /// must preserve that order and then append [`RawSeatEventKind::HostFocusLost`]
    /// at [`Self::last_event_time_msec`] so clients receive every end before
    /// Smithay clears pointer focus.
    pub fn cancel_active_input(&mut self) -> [Option<RawSeatEvent>; 2] {
        let gesture = self.active_gesture.cancel(self.last_event_time_msec);
        let scroll = (self.finger_axes.horizontal || self.finger_axes.vertical).then(|| {
            RawSeatEvent::new(
                RawSeatEventKind::PointerAxis {
                    position: Some(self.pointer),
                    axis: RawScrollFrame::cancelled_finger(
                        self.finger_axes.horizontal,
                        self.finger_axes.vertical,
                    ),
                },
                self.last_event_time_msec,
            )
        });
        self.finger_axes = ActiveScrollAxes::default();
        [gesture, scroll]
    }

    pub const fn last_event_time_msec(&self) -> u32 {
        self.last_event_time_msec
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

#[derive(Default)]
struct ActiveGesture(Option<PointerGestureKind>);

impl ActiveGesture {
    fn transition(&mut self, gesture: PointerGesture, time: u32) -> [Option<RawSeatEvent>; 2] {
        let kind = gesture.kind();
        if gesture.is_begin() {
            // Libinput normally serializes gestures. Keeping two fixed slots
            // repairs a stale stream without allocating on the input hot path.
            let previous = self.0.replace(kind);
            if let Some(previous) = previous {
                trace!(
                    ?previous,
                    ?kind,
                    "repaired a stale touchpad gesture before a new begin"
                );
            }
            let cancelled = previous.map(|active| pointer_gesture_event(active.cancelled(), time));
            let begin = pointer_gesture_event(gesture, time);
            return match cancelled {
                Some(end) => [Some(end), Some(begin)],
                None => single_event(Some(begin)),
            };
        }
        if self.0 != Some(kind) {
            trace!(
                active = ?self.0,
                ?kind,
                "dropped an unpaired touchpad gesture update or end"
            );
            return empty_batch();
        }
        if gesture.is_end() {
            self.0 = None;
        }
        single_event(Some(pointer_gesture_event(gesture, time)))
    }

    fn cancel(&mut self, time: u32) -> Option<RawSeatEvent> {
        self.0
            .take()
            .map(|kind| pointer_gesture_event(kind.cancelled(), time))
    }
}

const fn pointer_gesture_event(gesture: PointerGesture, time: u32) -> RawSeatEvent {
    RawSeatEvent::new(RawSeatEventKind::PointerGesture { gesture }, time)
}

const fn empty_batch() -> [Option<RawSeatEvent>; 2] {
    [None, None]
}

const fn single_event(event: Option<RawSeatEvent>) -> [Option<RawSeatEvent>; 2] {
    [event, None]
}

fn preferred_click_method(methods: &[ClickMethod]) -> Option<ClickMethod> {
    methods
        .contains(&ClickMethod::Clickfinger)
        .then_some(ClickMethod::Clickfinger)
}

fn configure_pointer_device(device: &mut Device) {
    configure_tapping(device);
    configure_clickfinger(device);
}

fn configure_tapping(device: &mut Device) {
    if device.config_tap_finger_count() == 0 {
        return;
    }
    let left_right_middle = match device.config_tap_set_button_map(TapButtonMap::LeftRightMiddle) {
        Ok(()) => true,
        Err(error) => {
            warn!(
                device = ?device.sysname(),
                ?error,
                "failed to set tap-to-click left/right/middle mapping; using the device mapping"
            );
            false
        }
    };
    if let Err(error) = device.config_tap_set_enabled(true) {
        warn!(
            device = ?device.sysname(),
            ?error,
            "failed to enable tap-to-click"
        );
        return;
    }
    if left_right_middle {
        debug!(
            device = ?device.sysname(),
            "enabled one/two/three-finger left/right/middle taps"
        );
    } else {
        debug!(device = ?device.sysname(), "enabled taps using the device button mapping");
    }
}

fn configure_clickfinger(device: &mut Device) {
    let Some(method) = preferred_click_method(&device.config_click_methods()) else {
        return;
    };
    if let Err(error) = device.config_click_set_method(method) {
        warn!(
            device = ?device.sysname(),
            ?error,
            "failed to enable clickfinger touchpad clicks"
        );
        return;
    }
    if let Err(error) =
        device.config_click_clickfinger_set_button_map(ClickfingerButtonMap::LeftRightMiddle)
    {
        warn!(
            device = ?device.sysname(),
            ?error,
            "failed to set clickfinger left/right/middle mapping"
        );
        return;
    }
    debug!(
        device = ?device.sysname(),
        "enabled one/two/three-finger left/right/middle clicks"
    );
}

#[cfg(test)]
mod tests {
    use smithay::reexports::input::ClickMethod;

    use super::{ActiveGesture, LibinputAdapter, preferred_click_method};
    use crate::input::{
        InputDelta, InputPosition, PointerGesture, PointerGestureKind, RawScrollPhase,
        RawScrollSource, RawSeatEvent, RawSeatEventKind, TouchpadHold, TouchpadPinch,
        TouchpadSwipe,
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

    #[test]
    fn clickfinger_is_selected_only_when_the_device_supports_it() {
        assert_eq!(
            preferred_click_method(&[ClickMethod::ButtonAreas, ClickMethod::Clickfinger]),
            Some(ClickMethod::Clickfinger)
        );
        assert_eq!(preferred_click_method(&[ClickMethod::ButtonAreas]), None);
    }

    #[test]
    fn gesture_lifecycle_repairs_stale_begins_and_drops_unpaired_events() {
        let mut active = ActiveGesture::default();
        let pinch_begin = PointerGesture::Pinch(TouchpadPinch::Begin { fingers: 2 });
        assert_eq!(
            active.transition(pinch_begin, 10),
            [
                Some(RawSeatEvent::new(
                    RawSeatEventKind::PointerGesture {
                        gesture: pinch_begin,
                    },
                    10,
                )),
                None,
            ]
        );

        let unmatched_swipe = PointerGesture::Swipe(TouchpadSwipe::Update {
            delta: InputDelta::new(1.0, 2.0),
        });
        assert_eq!(active.transition(unmatched_swipe, 11), [None, None]);
        assert_eq!(
            active.transition(
                PointerGesture::Hold(TouchpadHold::End { cancelled: false }),
                12,
            ),
            [None, None]
        );

        let hold_begin = PointerGesture::Hold(TouchpadHold::Begin { fingers: 3 });
        assert_eq!(
            active.transition(hold_begin, 13),
            [
                Some(RawSeatEvent::new(
                    RawSeatEventKind::PointerGesture {
                        gesture: PointerGestureKind::Pinch.cancelled(),
                    },
                    13,
                )),
                Some(RawSeatEvent::new(
                    RawSeatEventKind::PointerGesture {
                        gesture: hold_begin,
                    },
                    13,
                )),
            ]
        );
        assert_eq!(
            active.transition(
                PointerGesture::Hold(TouchpadHold::End { cancelled: false }),
                14,
            ),
            [
                Some(RawSeatEvent::new(
                    RawSeatEventKind::PointerGesture {
                        gesture: PointerGesture::Hold(TouchpadHold::End { cancelled: false }),
                    },
                    14,
                )),
                None,
            ]
        );
    }

    #[test]
    fn focus_loss_cancels_active_gesture_and_finger_scroll_with_libinput_time() {
        let mut adapter = LibinputAdapter::new(1920.0, 1080.0);
        adapter.active_gesture.0 = Some(PointerGestureKind::Swipe);
        adapter.finger_axes.horizontal = true;
        adapter.finger_axes.vertical = true;
        adapter.last_event_time_msec = 42;

        let cancelled = adapter.cancel_active_input();

        assert_eq!(
            cancelled[0],
            Some(RawSeatEvent::new(
                RawSeatEventKind::PointerGesture {
                    gesture: PointerGestureKind::Swipe.cancelled(),
                },
                42,
            ))
        );
        assert_eq!(
            cancelled[1],
            Some(RawSeatEvent::new(
                RawSeatEventKind::PointerAxis {
                    position: Some(InputPosition::new(960.0, 540.0)),
                    axis: crate::input::RawScrollFrame::cancelled_finger(true, true),
                },
                42,
            ))
        );
        assert_eq!(adapter.cancel_active_input(), [None, None]);
    }
}
