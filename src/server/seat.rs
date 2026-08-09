//! Smithay seat delivery and protocol focus application.

use bevy::input::ButtonState as BevyButtonState;
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState as SmithayButtonState, KeyState, Keycode},
    input::{
        Seat, SeatHandler,
        dnd::DndGrabHandler,
        keyboard::{FilterResult, KeyboardSource},
        pointer::{AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent},
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{Logical, SERIAL_COUNTER},
    wayland::{
        selection::{
            SelectionHandler,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
        },
        shell::xdg::ToplevelSurface,
    },
};
use tracing::{debug, warn};

use crate::{
    input::{SeatInputEffect, SeatInputEffectKind, SurfaceHit},
    raw_input::{InputPosition, RawScrollFrame, RawScrollSource},
    surface::SurfaceId,
};

use super::ServerState;

impl ServerState {
    pub(crate) fn apply_input_effect(&mut self, effect: SeatInputEffect) {
        let SeatInputEffect { event, time } = effect;
        match event {
            SeatInputEffectKind::PointerMotion { position, target } => {
                self.apply_pointer_motion(position, target, time)
            }
            SeatInputEffectKind::PointerButton {
                position,
                target,
                button,
                state,
            } => self.apply_pointer_button(position, target, button.0, state, time),
            SeatInputEffectKind::PointerAxis {
                position,
                target,
                axis,
            } => self.apply_pointer_axis(position, target, axis, time),
            SeatInputEffectKind::Keyboard { keycode, state } => {
                let Some(keycode) = keycode.0.checked_add(8) else {
                    warn!(keycode = keycode.0, "ignored an overflowing keyboard code");
                    return;
                };
                let Some(keyboard) = self.seat.get_keyboard() else {
                    warn!("ignored keyboard input because the seat has no keyboard");
                    return;
                };
                keyboard.input::<(), _>(
                    self,
                    Keycode::new(keycode),
                    smithay_key_state(state),
                    SERIAL_COUNTER.next_serial(),
                    time,
                    |_, _, _| FilterResult::Forward,
                );
            }
            SeatInputEffectKind::HostFocusLost => self.release_host_input(time),
        }
    }

    pub(super) fn focus_toplevel(&mut self, requested: Option<SurfaceId>) {
        let grabbed = self
            .seat
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed());
        let Some(requested) = transition_pending_focus(
            &mut self.pending_focus,
            grabbed,
            FocusTransition::Request(requested),
        ) else {
            debug!(?requested, "queued a focus request during a pointer grab");
            return;
        };
        self.apply_toplevel_focus(requested);
    }

    fn apply_toplevel_focus(&mut self, requested: Option<SurfaceId>) {
        let next = match requested {
            Some(id) => {
                let Some(toplevel) = self.toplevels.get(id) else {
                    warn!(?id, "ignored a focus request for an unknown surface");
                    return;
                };
                if !toplevel.surface.alive() {
                    warn!(?id, "ignored a focus request for a dead surface");
                    return;
                }
                Some((id, toplevel.surface.clone()))
            }
            None => None,
        };
        let previous = self.focused_toplevel.and_then(|id| {
            self.toplevels
                .get(id)
                .map(|toplevel| (id, toplevel.surface.clone()))
        });

        let next_id = next.as_ref().map(|(id, _)| *id);
        if self.focused_toplevel != next_id {
            if let Some((_, surface)) = &previous {
                set_activated(surface, false);
            }
            if let Some((_, surface)) = &next {
                set_activated(surface, true);
            }
            self.focused_toplevel = next_id;
        }

        let keyboard_focus = next
            .as_ref()
            .map(|(_, surface)| surface.wl_surface().clone());
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, keyboard_focus, SERIAL_COUNTER.next_serial());
        }
    }

    fn apply_pointer_motion(
        &mut self,
        position: InputPosition,
        target: Option<SurfaceHit>,
        time: u32,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            warn!("ignored pointer motion because the seat has no pointer");
            return;
        };
        self.pointer_position = position;
        let focus = self.pointer_focus(position, target);
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: compositor_point(position),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
        self.retry_pending_focus(pointer.is_grabbed());
    }

    fn apply_pointer_button(
        &mut self,
        position: InputPosition,
        target: Option<SurfaceHit>,
        button: u32,
        state: BevyButtonState,
        time: u32,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            warn!("ignored pointer button because the seat has no pointer");
            return;
        };
        self.pointer_position = position;
        let serial = SERIAL_COUNTER.next_serial();
        let focus = self.pointer_focus(position, target);
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: compositor_point(position),
                serial,
                time,
            },
        );
        match state {
            BevyButtonState::Pressed => {
                self.pressed_pointer_buttons.insert(button);
            }
            BevyButtonState::Released => {
                self.pressed_pointer_buttons.remove(&button);
            }
        }
        pointer.button(
            self,
            &ButtonEvent {
                serial,
                time,
                button,
                state: smithay_button_state(state),
            },
        );
        pointer.frame(self);
        self.retry_pending_focus(pointer.is_grabbed());
    }

    fn apply_pointer_axis(
        &mut self,
        position: InputPosition,
        target: Option<SurfaceHit>,
        axis: RawScrollFrame,
        time: u32,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            warn!("ignored pointer axis because the seat has no pointer");
            return;
        };
        self.pointer_position = position;
        let focus = self.pointer_focus(position, target);
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: compositor_point(position),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        if let Some(frame) = smithay_axis_frame(axis, time) {
            pointer.axis(self, frame);
        }
        pointer.frame(self);
        self.retry_pending_focus(pointer.is_grabbed());
    }

    fn pointer_focus(
        &self,
        position: InputPosition,
        target: Option<SurfaceHit>,
    ) -> Option<(WlSurface, smithay::utils::Point<f64, Logical>)> {
        let target = target?;
        let toplevel = self
            .toplevels
            .get(target.surface)
            .filter(|toplevel| toplevel.surface.alive())?;
        let origin = InputPosition::new(
            position.x - target.local_position.x,
            position.y - target.local_position.y,
        );
        Some((
            toplevel.surface.wl_surface().clone(),
            compositor_point(origin),
        ))
    }

    fn release_host_input(&mut self, time: u32) {
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(pointer) = self.seat.get_pointer() {
            for button in std::mem::take(&mut self.pressed_pointer_buttons) {
                pointer.button(
                    self,
                    &ButtonEvent {
                        serial,
                        time,
                        button,
                        state: SmithayButtonState::Released,
                    },
                );
            }
            pointer.motion(
                self,
                None,
                &MotionEvent {
                    location: compositor_point(self.pointer_position),
                    serial,
                    time,
                },
            );
            pointer.frame(self);
        } else {
            self.pressed_pointer_buttons.clear();
            warn!("could not release host pointer state because the seat has no pointer");
        }

        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.release_source(self, KeyboardSource::MAIN);
            keyboard.set_focus(self, None, serial);
        }
        transition_pending_focus(
            &mut self.pending_focus,
            false,
            FocusTransition::HostFocusLost,
        );
    }

    fn retry_pending_focus(&mut self, grabbed: bool) {
        let Some(requested) = transition_pending_focus(
            &mut self.pending_focus,
            grabbed,
            FocusTransition::PointerDelivered,
        ) else {
            return;
        };
        self.focus_toplevel(requested);
    }

    pub(super) fn clear_input_focus_for_surface(&mut self, surface: &WlSurface, time: u32) {
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(pointer) = self.seat.get_pointer()
            && pointer.current_focus().as_ref() == Some(surface)
        {
            pointer.motion(
                self,
                None,
                &MotionEvent {
                    location: compositor_point(self.pointer_position),
                    serial,
                    time,
                },
            );
            pointer.frame(self);
        }
        if let Some(keyboard) = self.seat.get_keyboard()
            && keyboard.current_focus().as_ref() == Some(surface)
        {
            keyboard.set_focus(self, None, serial);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum FocusTransition {
    Request(Option<SurfaceId>),
    PointerDelivered,
    HostFocusLost,
}

fn transition_pending_focus(
    pending: &mut Option<Option<SurfaceId>>,
    grabbed: bool,
    transition: FocusTransition,
) -> Option<Option<SurfaceId>> {
    match transition {
        FocusTransition::Request(requested) if grabbed => {
            *pending = Some(requested);
            None
        }
        FocusTransition::Request(requested) => {
            *pending = None;
            Some(requested)
        }
        FocusTransition::PointerDelivered if grabbed => None,
        FocusTransition::PointerDelivered => pending.take(),
        FocusTransition::HostFocusLost => {
            *pending = None;
            None
        }
    }
}

fn set_activated(surface: &ToplevelSurface, activated: bool) {
    if !surface.is_initial_configure_sent() {
        return;
    }
    surface.with_pending_state(|state| {
        if activated {
            state.states.set(xdg_toplevel::State::Activated);
        } else {
            state.states.unset(xdg_toplevel::State::Activated);
        }
    });
    surface.send_pending_configure();
}

impl SeatHandler for ServerState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut smithay::input::SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|surface| self.display_handle.get_client(surface.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}

impl SelectionHandler for ServerState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for ServerState {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DndGrabHandler for ServerState {}
impl WaylandDndGrabHandler for ServerState {}

fn compositor_point(position: InputPosition) -> smithay::utils::Point<f64, Logical> {
    (position.x, position.y).into()
}

const fn smithay_key_state(state: BevyButtonState) -> KeyState {
    match state {
        BevyButtonState::Pressed => KeyState::Pressed,
        BevyButtonState::Released => KeyState::Released,
    }
}

const fn smithay_button_state(state: BevyButtonState) -> SmithayButtonState {
    match state {
        BevyButtonState::Pressed => SmithayButtonState::Pressed,
        BevyButtonState::Released => SmithayButtonState::Released,
    }
}

fn smithay_axis_frame(axis: RawScrollFrame, time: u32) -> Option<AxisFrame> {
    if axis.horizontal == 0.0
        && axis.vertical == 0.0
        && axis.horizontal_v120.unwrap_or_default() == 0
        && axis.vertical_v120.unwrap_or_default() == 0
    {
        return None;
    }
    let source = match axis.source {
        RawScrollSource::Wheel => AxisSource::Wheel,
        RawScrollSource::Continuous => AxisSource::Continuous,
    };
    let mut frame = AxisFrame::new(time).source(source);
    if axis.horizontal != 0.0 {
        frame = frame.value(Axis::Horizontal, axis.horizontal);
    }
    if axis.vertical != 0.0 {
        frame = frame.value(Axis::Vertical, axis.vertical);
    }
    if let Some(v120) = axis.horizontal_v120
        && v120 != 0
    {
        frame = frame.v120(Axis::Horizontal, v120);
    }
    if let Some(v120) = axis.vertical_v120
        && v120 != 0
    {
        frame = frame.v120(Axis::Vertical, v120);
    }
    Some(frame)
}

#[cfg(test)]
mod tests {
    use crate::{
        raw_input::{RawScrollFrame, RawScrollSource},
        surface::SurfaceId,
    };

    use super::{FocusTransition, smithay_axis_frame, transition_pending_focus};

    #[test]
    fn focus_requests_apply_immediately_without_a_grab() {
        let surface = SurfaceId::new(1);
        let mut pending = Some(None);

        assert_eq!(
            transition_pending_focus(&mut pending, false, FocusTransition::Request(Some(surface)),),
            Some(Some(surface)),
        );
        assert_eq!(pending, None);
    }

    #[test]
    fn grabbed_focus_requests_queue_with_last_request_winning() {
        let first = SurfaceId::new(1);
        let second = SurfaceId::new(2);
        let mut pending = None;

        assert_eq!(
            transition_pending_focus(&mut pending, true, FocusTransition::Request(Some(first)),),
            None,
        );
        assert_eq!(pending, Some(Some(first)));
        assert_eq!(
            transition_pending_focus(&mut pending, true, FocusTransition::Request(Some(second)),),
            None,
        );
        assert_eq!(pending, Some(Some(second)));
    }

    #[test]
    fn queued_focus_applies_once_after_the_grab_ends() {
        let surface = SurfaceId::new(1);
        let mut pending = Some(Some(surface));

        assert_eq!(
            transition_pending_focus(&mut pending, true, FocusTransition::PointerDelivered,),
            None,
        );
        assert_eq!(pending, Some(Some(surface)));
        assert_eq!(
            transition_pending_focus(&mut pending, false, FocusTransition::PointerDelivered,),
            Some(Some(surface)),
        );
        assert_eq!(pending, None);
        assert_eq!(
            transition_pending_focus(&mut pending, false, FocusTransition::PointerDelivered,),
            None,
        );
    }

    #[test]
    fn host_focus_loss_discards_a_queued_focus_request() {
        let mut pending = Some(Some(SurfaceId::new(1)));

        assert_eq!(
            transition_pending_focus(&mut pending, false, FocusTransition::HostFocusLost,),
            None,
        );
        assert_eq!(pending, None);
    }

    #[test]
    fn skips_empty_axis_frames() {
        assert!(
            smithay_axis_frame(
                RawScrollFrame {
                    source: RawScrollSource::Wheel,
                    horizontal: 0.0,
                    vertical: 0.0,
                    horizontal_v120: Some(0),
                    vertical_v120: None,
                },
                1,
            )
            .is_none()
        );
    }
}
