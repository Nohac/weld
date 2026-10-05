//! Smithay seat delivery and protocol focus application.

use std::{borrow::Cow, cell::RefCell};

use smithay::{
    backend::input::{
        Axis, AxisSource, ButtonState as SmithayButtonState, InputTime, KeyEvent, Keycode,
    },
    input::{
        Seat, SeatHandler,
        dnd::{DnDGrab, DndGrabHandler, GrabType, Source},
        keyboard::{FilterResult, KeyboardSource, LegacyRepeat, RepeatMode},
        pointer::{
            AxisFrame, ButtonEvent, CursorImageStatus, Focus, GestureHoldBeginEvent,
            GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
            GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
            GestureSwipeUpdateEvent, GrabStartData, MotionEvent, PointerGrab, PointerHandle,
            PointerInnerHandle, RelativeMotionEvent,
        },
    },
    reexports::wayland_server::{
        Resource,
        protocol::{wl_seat, wl_surface::WlSurface},
    },
    utils::{Logical, SERIAL_COUNTER},
    wayland::{
        pointer_constraints::PointerConstraintsHandler,
        seat::WaylandFocus,
        selection::{
            SelectionHandler,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
        },
        shell::xdg::ToplevelSurface,
    },
};
use tracing::{debug, trace, warn};
use weld_client::{
    ClientInputEvent, ClientInputTarget, InputEventKind, KeyboardKeyState, RelativeMotion,
};

use crate::{
    input::{
        ButtonState, InputDelta, InputPosition, KeyboardRepeatMode, LegacyKeyRepeat,
        PointerGesture, RawScrollFrame, RawScrollSource, SurfaceHit, TouchpadHold, TouchpadPinch,
        TouchpadSwipe,
    },
    surface::{SurfaceId, WindowDecoration, WindowInteractionRequestKind, WindowResizeEdge},
};

use super::input_seat::InputSeat;
use super::keyboard_focus::KeyboardFocus;
use super::{PendingSurfaceEvent, PendingSurfaceEventKind, ServerState};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OrdinaryImplicitGrab {
    owner: Option<SurfaceId>,
}

impl ServerState {
    fn clear_selection_focus(&mut self, seat: &Seat<Self>) {
        let group = self.data_device_seat(seat);
        if let Some(owner) = group
            .user_data()
            .get::<super::seat_bindings::SelectionFocus>()
            && owner
                .0
                .borrow()
                .as_ref()
                .and_then(smithay::input::WeakSeat::upgrade)
                .as_ref()
                == Some(seat)
        {
            owner.0.borrow_mut().take();
            set_data_device_focus(&self.display_handle, &group, None);
        }
    }
    pub(super) fn apply_client_input(&mut self, input: ClientInputEvent) {
        self.route_controller_input(None, input);
    }

    pub(super) fn deliver_client_input(&mut self, input_seat: &InputSeat, input: ClientInputEvent) {
        let ClientInputEvent {
            target,
            host_position,
            event,
            time,
        } = input;
        if target.surface().source() != crate::WAYLAND_CLIENT_SOURCE {
            warn!(surface = ?target.surface(), "ignored client input addressed to another source");
            return;
        }
        match (target, event) {
            (
                ClientInputTarget::Pointer { surface, layer },
                InputEventKind::PointerMotion { position, relative },
            ) => {
                let host_position = host_position.unwrap_or(input_seat.pointer_position.get());
                self.apply_pointer_motion(
                    input_seat,
                    host_position,
                    Some(SurfaceHit {
                        surface,
                        layer,
                        local_position: position,
                    }),
                    relative,
                    time,
                );
            }
            (ClientInputTarget::Pointer { .. }, InputEventKind::PointerLeft { .. }) => {
                self.apply_pointer_motion(
                    input_seat,
                    host_position.unwrap_or(input_seat.pointer_position.get()),
                    None,
                    None,
                    time,
                );
            }
            (
                ClientInputTarget::Pointer { surface, layer },
                InputEventKind::PointerButton {
                    position,
                    button,
                    state,
                },
            ) => {
                let target = position.map(|local_position| SurfaceHit {
                    surface,
                    layer,
                    local_position,
                });
                self.apply_pointer_button(
                    input_seat,
                    host_position.unwrap_or(input_seat.pointer_position.get()),
                    target,
                    button.0,
                    state,
                    time,
                );
            }
            (ClientInputTarget::Pointer { .. }, InputEventKind::PointerAxis { axis, .. }) => {
                self.apply_pointer_axis(input_seat, axis, time)
            }
            (ClientInputTarget::Pointer { .. }, InputEventKind::PointerGesture { gesture }) => {
                self.apply_pointer_gesture(input_seat, gesture, time)
            }
            (
                ClientInputTarget::Keyboard { surface },
                InputEventKind::Keyboard { keycode, state },
            ) => self.apply_keyboard_input(input_seat, surface, keycode, state, time),
            (target, event) => {
                warn!(
                    ?target,
                    ?event,
                    "ignored client input with a mismatched target kind"
                );
            }
        }
    }

    fn apply_keyboard_input(
        &mut self,
        input_seat: &InputSeat,
        surface: SurfaceId,
        keycode: crate::input::LinuxKeycode,
        state: KeyboardKeyState,
        time: u32,
    ) {
        let Some(native_keycode) = keycode.0.checked_add(8) else {
            warn!(keycode = keycode.0, "ignored an overflowing keyboard code");
            return;
        };
        let Some(keyboard) = input_seat.native.get_keyboard() else {
            warn!("ignored keyboard input because the seat has no keyboard");
            return;
        };
        if !input_seat
            .keyboard_repeats
            .borrow_mut()
            .observe(surface, keycode, state)
            || (state == KeyboardKeyState::Repeated
                && input_seat.controller.get().is_none()
                && self.keyboard_repeat_mode != KeyboardRepeatMode::Compositor)
        {
            trace!(
                ?surface,
                "ignored keyboard repeat outside its original input context"
            );
            return;
        }
        if input_seat.keyboard_diagnostic_dirty.get()
            && let Some(client) = keyboard
                .current_focus()
                .and_then(|surface| surface.wl_surface().and_then(|surface| surface.client()))
        {
            let versions: Vec<_> = keyboard
                .client_keyboards(&client)
                .map(|keyboard| keyboard.version())
                .collect();
            tracing::info!(?surface, ?versions, repeat_mode = ?self.keyboard_repeat_mode,
                legacy_repeat = ?self.legacy_key_repeat, "focused client keyboard repeat support");
            input_seat.keyboard_diagnostic_dirty.set(false);
        }
        keyboard.input::<(), _>(
            self,
            Keycode::new(native_keycode),
            smithay_key_state(state),
            SERIAL_COUNTER.next_serial(),
            InputTime::from_millis(time),
            |_, _, _| FilterResult::Forward,
        );
    }

    pub(crate) fn set_legacy_key_repeat(&mut self, legacy: LegacyKeyRepeat) {
        if self.legacy_key_repeat != legacy {
            self.legacy_key_repeat = legacy;
            for input in self.input_seats() {
                input.keyboard_diagnostic_dirty.set(true);
            }
            if legacy != LegacyKeyRepeat::Client
                && self.keyboard_repeat_mode == KeyboardRepeatMode::Client
            {
                warn!(
                    ?legacy,
                    "legacy repeat fallback has no effect in client repeat mode; the workaround requires compositor repeat mode"
                );
            }
            self.configure_keyboard_repeat();
        }
    }

    pub(super) fn configure_keyboard_repeat(&mut self) {
        for input in self.input_seats().cloned().collect::<Vec<_>>() {
            self.configure_seat_repeat(&input);
        }
    }

    pub(super) fn configure_seat_repeat(&mut self, input_seat: &InputSeat) {
        let Some(keyboard) = input_seat.native.get_keyboard() else {
            return;
        };
        let mode = match if input_seat.controller.get().is_some() {
            KeyboardRepeatMode::Compositor
        } else {
            self.keyboard_repeat_mode
        } {
            KeyboardRepeatMode::Client => RepeatMode::Client,
            KeyboardRepeatMode::Compositor => RepeatMode::Compositor {
                legacy_repeat: match self.legacy_key_repeat {
                    LegacyKeyRepeat::Client => LegacyRepeat::Client,
                    LegacyKeyRepeat::Disabled => LegacyRepeat::Disabled,
                    LegacyKeyRepeat::Emulated => LegacyRepeat::Emulated,
                },
            },
        };
        keyboard.change_repeat_mode(self, mode);
    }

    pub(super) fn focus_toplevel(&mut self, requested: Option<SurfaceId>) {
        self.route_controller_focus(None, requested);
    }

    pub(super) fn focus_seat(&mut self, input_seat: &InputSeat, requested: Option<SurfaceId>) {
        if requested.is_none() {
            self.dismiss_popup_grab(input_seat, self.event_time());
        }
        let grabbed = input_seat
            .native
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed());
        let grabbed = focus_request_remains_protected(
            grabbed,
            input_seat.ordinary_implicit_grab.get(),
            requested,
        );
        let Some(requested) = transition_pending_focus(
            &mut input_seat.pending_focus.borrow_mut(),
            grabbed,
            FocusTransition::Request(requested),
        ) else {
            debug!(?requested, "queued a focus request during a pointer grab");
            return;
        };
        self.apply_toplevel_focus(input_seat, requested);
    }

    pub(super) fn begin_pointer_move(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: smithay::utils::Serial,
    ) {
        self.begin_pointer_interaction(surface, seat, serial, PointerInteraction::Move);
    }

    pub(super) fn begin_pointer_resize(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: smithay::utils::Serial,
        edges: WindowResizeEdge,
    ) {
        self.begin_pointer_interaction(surface, seat, serial, PointerInteraction::Resize(edges));
    }

    fn begin_pointer_interaction(
        &mut self,
        surface: ToplevelSurface,
        seat_resource: wl_seat::WlSeat,
        serial: smithay::utils::Serial,
        interaction: PointerInteraction,
    ) {
        let Some(seat) = Seat::<Self>::from_resource(&seat_resource) else {
            return;
        };
        let Some(input_seat) = self.input_for_native(&seat) else {
            return;
        };
        let Some(pointer) = seat.get_pointer() else {
            return;
        };
        if !pointer.has_grab(serial) {
            return;
        }
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        let Some((focused, _)) = &start_data.focus else {
            return;
        };
        if !focused.same_client_as(&surface.wl_surface().id()) {
            return;
        }
        let Some(surface_id) = self.toplevels.id_for_surface(surface.wl_surface()) else {
            return;
        };
        let Some(toplevel) = self.toplevels.get(surface_id) else {
            return;
        };
        if toplevel.decoration != WindowDecoration::ClientSide {
            return;
        }

        self.install_window_grab(
            &input_seat,
            surface_id,
            pointer,
            start_data,
            serial,
            interaction,
        );
    }

    pub(super) fn begin_x11_interaction(
        &mut self,
        surface_id: SurfaceId,
        button: u32,
        edges: Option<WindowResizeEdge>,
    ) {
        let Some(window) = self.toplevels.get(surface_id) else {
            return;
        };
        let linux_button = match button {
            1 => 0x110,
            2 => 0x112,
            3 => 0x111,
            _ => return,
        };
        let candidate = self.input_seats().find_map(|input| {
            if !input
                .pressed_pointer_buttons
                .borrow()
                .contains(&linux_button)
            {
                return None;
            }
            let pointer = input.native.get_pointer()?;
            let start_data = pointer.grab_start_data()?;
            let (focused, _) = start_data.focus.as_ref()?;
            (super::surface_tree::owning_root(focused) == *window.surface.wl_surface())
                .then(|| (input.clone(), pointer, start_data))
        });
        let Some((input_seat, pointer, start_data)) = candidate else {
            return;
        };
        let interaction = edges.map_or(PointerInteraction::Move, PointerInteraction::Resize);
        self.install_window_grab(
            &input_seat,
            surface_id,
            pointer,
            start_data,
            SERIAL_COUNTER.next_serial(),
            interaction,
        );
    }

    fn install_window_grab(
        &mut self,
        input_seat: &InputSeat,
        surface_id: SurfaceId,
        pointer: PointerHandle<Self>,
        start_data: GrabStartData<Self>,
        serial: smithay::utils::Serial,
        interaction: PointerInteraction,
    ) {
        let request = match interaction {
            PointerInteraction::Move => WindowInteractionRequestKind::Move,
            PointerInteraction::Resize(edges) => WindowInteractionRequestKind::Resize { edges },
        };
        pointer.set_grab(
            self,
            WindowProtocolGrab {
                start_data,
                surface_id,
                resizing: matches!(interaction, PointerInteraction::Resize(_)),
            },
            serial,
            Focus::Clear,
        );
        input_seat.ordinary_implicit_grab.set(None);
        // Installing a grab unsets any previous grab. Stage the new resize state
        // afterwards so replacing a grab cannot clear the state we just entered.
        if matches!(interaction, PointerInteraction::Resize(_)) {
            self.begin_protocol_resize(surface_id);
        }
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: surface_id,
            kind: PendingSurfaceEventKind::WindowInteraction(request),
        });
    }

    fn apply_toplevel_focus(&mut self, input_seat: &InputSeat, requested: Option<SurfaceId>) {
        if let Some(layer) = requested.and_then(|id| self.layers.0.get(id)) {
            if !layer.surface.layer_surface().alive()
                || !layer.tree.client_mapped(layer.surface.wl_surface())
                || !layer.surface.can_receive_keyboard_focus()
            {
                warn!(
                    ?requested,
                    "ignored keyboard focus for an unavailable or noninteractive layer surface"
                );
                return;
            }
            let surface = layer.surface.wl_surface().clone();
            if let Some(previous_id) = input_seat.focused_toplevel.take()
                && !self
                    .input_seats()
                    .any(|other| other.focused_toplevel.get() == Some(previous_id))
                && let Some(previous) = self.toplevels.get(previous_id)
            {
                previous.surface.set_activated(false);
            }
            if let Some(keyboard) = input_seat.native.get_keyboard() {
                keyboard.set_focus(self, Some(surface.into()), SERIAL_COUNTER.next_serial());
            }
            return;
        }
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
        let previous = input_seat.focused_toplevel.get().and_then(|id| {
            self.toplevels
                .get(id)
                .map(|toplevel| (id, toplevel.surface.clone()))
        });

        let next_id = next.as_ref().map(|(id, _)| *id);
        if input_seat.focused_toplevel.get() != next_id {
            if let Some((previous_id, surface)) = &previous
                && !self.input_seats().any(|other| {
                    other.native != input_seat.native
                        && other.focused_toplevel.get() == Some(*previous_id)
                })
            {
                surface.set_activated(false);
            }
            if let Some((_, surface)) = &next {
                self.raise_x11_surface(surface.wl_surface());
                surface.set_activated(true);
            }
            input_seat.focused_toplevel.set(next_id);
        }

        let keyboard_focus = next
            .as_ref()
            .map(|(_, surface)| KeyboardFocus::from(surface));
        if keyboard_focus.is_none() {
            self.clear_selection_focus(&input_seat.native);
        }
        if let Some(keyboard) = input_seat.native.get_keyboard() {
            keyboard.set_focus(self, keyboard_focus, SERIAL_COUNTER.next_serial());
        }
    }

    fn apply_pointer_motion(
        &mut self,
        input_seat: &InputSeat,
        position: InputPosition,
        target: Option<SurfaceHit>,
        relative: Option<RelativeMotion>,
        time: u32,
    ) {
        let Some(pointer) = input_seat.native.get_pointer() else {
            warn!("ignored pointer motion because the seat has no pointer");
            return;
        };
        input_seat.pointer_position.set(position);
        let focus = self.pointer_focus(position, target);
        if !pointer.is_grabbed()
            && let Some((surface, _)) = &focus
        {
            self.raise_x11_surface(surface);
        }
        let shell_owns_cursor = shell_owns_cursor(
            focus.is_none(),
            pointer.is_grabbed(),
            input_seat.ordinary_implicit_grab.get(),
        );
        pointer.motion(
            self,
            focus.clone(),
            &MotionEvent {
                location: compositor_point(position),
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::from_millis(time),
            },
        );
        if let Some(relative) = relative.filter(|motion| motion.is_finite()) {
            pointer.relative_motion(
                self,
                focus,
                &RelativeMotionEvent {
                    delta: (relative.delta.x, relative.delta.y).into(),
                    delta_unaccel: (relative.unaccelerated.x, relative.unaccelerated.y).into(),
                    time: InputTime::from_micros(relative.time_micros),
                },
            );
        }
        pointer.frame(self);
        self.set_shell_cursor_ownership(input_seat, shell_owns_cursor);
        self.retry_pending_focus(input_seat, pointer.is_grabbed());
    }

    fn apply_pointer_button(
        &mut self,
        input_seat: &InputSeat,
        position: InputPosition,
        target: Option<SurfaceHit>,
        button: u32,
        state: ButtonState,
        time: u32,
    ) {
        let Some(pointer) = input_seat.native.get_pointer() else {
            warn!("ignored pointer button because the seat has no pointer");
            return;
        };
        if state == ButtonState::Released
            && !input_seat
                .pressed_pointer_buttons
                .borrow()
                .contains(&button)
        {
            return;
        }
        input_seat.pointer_position.set(position);
        let serial = SERIAL_COUNTER.next_serial();
        let focus = self.pointer_focus(position, target);
        let pointer_was_grabbed = pointer.is_grabbed();
        if !pointer_was_grabbed && let Some((surface, _)) = &focus {
            self.raise_x11_surface(surface);
        }
        debug!(
            target: "weld_input_diag",
            time, button, ?state, ?target,
            resolved_surface = ?focus.as_ref().map(|(surface, _)| surface.id()),
            current_surface = ?pointer.current_focus().map(|surface| surface.id()),
            pointer_was_grabbed,
            popup_grab_active = input_seat.popup_grab.borrow().as_ref().is_some_and(|grab| !grab.has_ended()),
            modifiers = ?input_seat.native.get_keyboard().map(|keyboard| keyboard.modifier_state()),
            "source pointer button before delivery"
        );
        let shell_owns_cursor = shell_owns_cursor(
            focus.is_none(),
            pointer_was_grabbed,
            input_seat.ordinary_implicit_grab.get(),
        );
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: compositor_point(position),
                serial,
                time: InputTime::from_millis(time),
            },
        );
        match state {
            ButtonState::Pressed => {
                if input_seat.pressed_pointer_buttons.borrow_mut().is_empty()
                    && !pointer_was_grabbed
                {
                    input_seat
                        .ordinary_implicit_grab
                        .set(Some(OrdinaryImplicitGrab {
                            owner: target.map(|target| target.surface),
                        }));
                }
                input_seat
                    .pressed_pointer_buttons
                    .borrow_mut()
                    .insert(button);
            }
            ButtonState::Released => {
                input_seat
                    .pressed_pointer_buttons
                    .borrow_mut()
                    .remove(&button);
                if input_seat.pressed_pointer_buttons.borrow_mut().is_empty() {
                    input_seat.ordinary_implicit_grab.set(None);
                }
            }
        }
        pointer.button(
            self,
            &ButtonEvent {
                serial,
                time: InputTime::from_millis(time),
                button,
                state: smithay_button_state(state),
            },
        );
        pointer.frame(self);
        debug!(
            target: "weld_input_diag",
            time, button, ?state,
            current_surface = ?pointer.current_focus().map(|surface| surface.id()),
            grabbed = pointer.is_grabbed(),
            pressed_buttons = ?input_seat.pressed_pointer_buttons.borrow_mut(),
            "source pointer button after delivery"
        );
        self.set_shell_cursor_ownership(input_seat, shell_owns_cursor);
        self.retry_pending_focus(input_seat, pointer.is_grabbed());
    }

    fn apply_pointer_axis(&mut self, input_seat: &InputSeat, axis: RawScrollFrame, time: u32) {
        trace!(?axis, "delivering pointer axis to Smithay's current focus");
        let Some(pointer) = input_seat.native.get_pointer() else {
            warn!("ignored pointer axis because the seat has no pointer");
            return;
        };
        let Some(frame) = smithay_axis_frame(axis, time) else {
            return;
        };
        if axis.source == RawScrollSource::Finger {
            input_seat.finger_scroll.set(!matches!(
                axis.phase,
                crate::input::RawScrollPhase::Ended | crate::input::RawScrollPhase::Cancelled
            ));
        }
        pointer.axis(self, frame);
        pointer.frame(self);
        self.retry_pending_focus(input_seat, pointer.is_grabbed());
    }

    fn apply_pointer_gesture(
        &mut self,
        input_seat: &InputSeat,
        gesture: PointerGesture,
        time: u32,
    ) {
        let Some(pointer) = input_seat.native.get_pointer() else {
            warn!("ignored touchpad gesture because the seat has no pointer");
            return;
        };
        if gesture.is_begin() {
            input_seat.active_gesture.set(Some(gesture.kind()));
        } else if gesture.is_end() {
            input_seat.active_gesture.set(None);
        }
        match gesture {
            PointerGesture::Swipe(TouchpadSwipe::Begin { fingers }) => {
                pointer.gesture_swipe_begin(
                    self,
                    &GestureSwipeBeginEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: InputTime::from_millis(time),
                        fingers,
                    },
                );
            }
            PointerGesture::Swipe(TouchpadSwipe::Update { delta }) => {
                pointer.gesture_swipe_update(
                    self,
                    &GestureSwipeUpdateEvent {
                        time: InputTime::from_millis(time),
                        delta: gesture_delta(delta),
                    },
                );
            }
            PointerGesture::Swipe(TouchpadSwipe::End { cancelled }) => {
                pointer.gesture_swipe_end(
                    self,
                    &GestureSwipeEndEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: InputTime::from_millis(time),
                        cancelled,
                    },
                );
            }
            PointerGesture::Pinch(TouchpadPinch::Begin { fingers }) => {
                pointer.gesture_pinch_begin(
                    self,
                    &GesturePinchBeginEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: InputTime::from_millis(time),
                        fingers,
                    },
                );
            }
            PointerGesture::Pinch(TouchpadPinch::Update {
                delta,
                scale,
                rotation,
            }) => {
                pointer.gesture_pinch_update(
                    self,
                    &GesturePinchUpdateEvent {
                        time: InputTime::from_millis(time),
                        delta: gesture_delta(delta),
                        scale,
                        rotation,
                    },
                );
            }
            PointerGesture::Pinch(TouchpadPinch::End { cancelled }) => {
                pointer.gesture_pinch_end(
                    self,
                    &GesturePinchEndEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: InputTime::from_millis(time),
                        cancelled,
                    },
                );
            }
            PointerGesture::Hold(TouchpadHold::Begin { fingers }) => {
                pointer.gesture_hold_begin(
                    self,
                    &GestureHoldBeginEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: InputTime::from_millis(time),
                        fingers,
                    },
                );
            }
            PointerGesture::Hold(TouchpadHold::End { cancelled }) => {
                pointer.gesture_hold_end(
                    self,
                    &GestureHoldEndEvent {
                        serial: SERIAL_COUNTER.next_serial(),
                        time: InputTime::from_millis(time),
                        cancelled,
                    },
                );
            }
        }
    }

    pub(super) fn pointer_focus(
        &self,
        position: InputPosition,
        target: Option<SurfaceHit>,
    ) -> Option<(WlSurface, smithay::utils::Point<f64, Logical>)> {
        let target = target?;
        let tree = if let Some(toplevel) = self
            .toplevels
            .get(target.surface)
            .filter(|toplevel| toplevel.surface.alive())
        {
            &toplevel.tree
        } else if let Some(layer) = self
            .layers
            .0
            .get(target.surface)
            .filter(|layer| layer.surface.layer_surface().alive())
        {
            &layer.tree
        } else {
            &self
                .popups
                .get(target.surface)
                .filter(|popup| popup.surface.alive())?
                .tree
        };
        let input_surface =
            tree.input_surface(target.layer, compositor_point(target.local_position))?;
        let origin = InputPosition::new(
            position.x - target.local_position.x,
            position.y - target.local_position.y,
        );
        Some((input_surface, compositor_point(origin)))
    }

    pub(super) fn release_host_input(&mut self, time: u32) {
        self.release_controller_input(None, time);
    }

    pub(super) fn release_seat_input(&mut self, input_seat: &InputSeat, time: u32) {
        self.cancel_touch_context(input_seat, time);
        self.clear_selection_focus(&input_seat.native);
        input_seat.keyboard_repeats.borrow_mut().clear();
        self.dismiss_popup_grab(input_seat, time);
        self.release_seat_pointer(input_seat, time);
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = input_seat.native.get_keyboard() {
            keyboard.release_source(self, KeyboardSource::MAIN);
            keyboard.set_focus(self, None, serial);
        }
        transition_pending_focus(
            &mut input_seat.pending_focus.borrow_mut(),
            false,
            FocusTransition::HostFocusLost,
        );
    }

    fn release_seat_pointer(&mut self, input_seat: &InputSeat, time: u32) {
        if let Some(gesture) = input_seat.active_gesture.take() {
            self.apply_pointer_gesture(input_seat, gesture.cancelled(), time);
        }
        if input_seat.finger_scroll.replace(false) {
            self.apply_pointer_axis(
                input_seat,
                RawScrollFrame::cancelled_finger(true, true),
                time,
            );
        }
        input_seat.ordinary_implicit_grab.set(None);
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(pointer) = input_seat.native.get_pointer() {
            let buttons = std::mem::take(&mut *input_seat.pressed_pointer_buttons.borrow_mut());
            for button in buttons {
                pointer.button(
                    self,
                    &ButtonEvent {
                        serial,
                        time: InputTime::from_millis(time),
                        button,
                        state: SmithayButtonState::Released,
                    },
                );
            }
            pointer.motion(
                self,
                None,
                &MotionEvent {
                    location: compositor_point(input_seat.pointer_position.get()),
                    serial,
                    time: InputTime::from_millis(time),
                },
            );
            pointer.frame(self);
        } else {
            input_seat.pressed_pointer_buttons.borrow_mut().clear();
            warn!("could not release host pointer state because the seat has no pointer");
        }
    }

    fn retry_pending_focus(&mut self, input_seat: &InputSeat, grabbed: bool) {
        let Some(requested) = transition_pending_focus(
            &mut input_seat.pending_focus.borrow_mut(),
            grabbed,
            FocusTransition::PointerDelivered,
        ) else {
            return;
        };
        self.focus_seat(input_seat, requested);
    }

    pub(super) fn retire_input_target(&mut self, surface: SurfaceId, time: u32) {
        self.retire_touch_target(surface, time);
        let inputs = self.input_seats().cloned().collect::<Vec<_>>();
        for input in inputs {
            input.admission.borrow_mut().retire_surface(surface);
            if input.desired_focus.get() == Some(surface) {
                input.desired_focus.set(None);
            }
            if input.focused_toplevel.get() == Some(surface) {
                self.release_seat_input(&input, time);
                self.focus_seat(&input, None);
            } else {
                let keys = input.keyboard_repeats.borrow().keys_for_surface(surface);
                for key in keys {
                    self.apply_keyboard_input(
                        &input,
                        surface,
                        key,
                        KeyboardKeyState::Released,
                        time,
                    );
                }
                if input
                    .ordinary_implicit_grab
                    .get()
                    .is_some_and(|grab| grab.owner == Some(surface))
                {
                    self.release_seat_pointer(&input, time);
                }
            }
        }
    }

    pub(super) fn clear_input_focus_for_surface(&mut self, surface: &WlSurface, time: u32) {
        self.retire_touch_surface(surface, time);
        if let Some(id) = self
            .toplevels
            .id_for_surface(surface)
            .or_else(|| self.popups.id_for_surface(surface))
            .or_else(|| self.layers.id_for_surface(surface))
        {
            self.retire_input_target(id, time);
        }
        for input in self.input_seats().cloned().collect::<Vec<_>>() {
            self.clear_seat_focus_for_surface(&input, surface, time);
        }
    }

    pub(super) fn clear_seat_pointer(&mut self, input_seat: &InputSeat, time: u32) {
        self.apply_pointer_motion(
            input_seat,
            input_seat.pointer_position.get(),
            None,
            None,
            time,
        );
    }

    fn clear_seat_focus_for_surface(
        &mut self,
        input_seat: &InputSeat,
        surface: &WlSurface,
        time: u32,
    ) {
        let pointer_matches = input_seat
            .native
            .get_pointer()
            .and_then(|pointer| pointer.current_focus())
            .as_ref()
            == Some(surface);
        let keyboard_matches = input_seat
            .native
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .and_then(|focus| focus.wl_surface().map(Cow::into_owned))
            .as_ref()
            == Some(surface);
        if pointer_matches {
            self.release_seat_pointer(input_seat, time);
        }
        let popup_ended = input_seat
            .popup_grab
            .borrow()
            .as_ref()
            .is_some_and(|grab| grab.has_ended());
        if popup_ended {
            self.dismiss_popup_grab(input_seat, time);
        }
        if keyboard_matches {
            self.clear_selection_focus(&input_seat.native);
            if let Some(keyboard) = input_seat.native.get_keyboard() {
                keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
            }
        }
    }
}

#[derive(Clone, Copy)]
enum PointerInteraction {
    Move,
    Resize(WindowResizeEdge),
}

struct WindowProtocolGrab {
    start_data: GrabStartData<ServerState>,
    surface_id: SurfaceId,
    resizing: bool,
}

impl PointerGrab<ServerState> for WindowProtocolGrab {
    fn motion(
        &mut self,
        data: &mut ServerState,
        handle: &mut PointerInnerHandle<'_, ServerState>,
        _focus: Option<(WlSurface, smithay::utils::Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
    }

    fn relative_motion(
        &mut self,
        data: &mut ServerState,
        handle: &mut PointerInnerHandle<'_, ServerState>,
        focus: Option<(WlSurface, smithay::utils::Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut ServerState,
        handle: &mut PointerInnerHandle<'_, ServerState>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if !handle.current_pressed().contains(&self.start_data.button) {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut ServerState,
        handle: &mut PointerInnerHandle<'_, ServerState>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut ServerState, handle: &mut PointerInnerHandle<'_, ServerState>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut ServerState,
        handle: &mut PointerInnerHandle<'_, ServerState>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut ServerState,
        handle: &mut PointerInnerHandle<'_, ServerState>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut ServerState,
        handle: &mut PointerInnerHandle<'_, ServerState>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut ServerState,
        handle: &mut PointerInnerHandle<'_, ServerState>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut ServerState,
        handle: &mut PointerInnerHandle<'_, ServerState>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut ServerState,
        handle: &mut PointerInnerHandle<'_, ServerState>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut ServerState,
        handle: &mut PointerInnerHandle<'_, ServerState>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut ServerState,
        handle: &mut PointerInnerHandle<'_, ServerState>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &GrabStartData<ServerState> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut ServerState) {
        if self.resizing {
            let pending = data.take_pending_resize(self.surface_id);
            data.finish_protocol_resize(self.surface_id, pending);
        }
        data.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: self.surface_id,
            kind: PendingSurfaceEventKind::WindowInteraction(WindowInteractionRequestKind::End),
        });
    }
}

#[derive(Clone, Copy, Debug)]
enum FocusTransition {
    Request(Option<SurfaceId>),
    PointerDelivered,
    HostFocusLost,
}

fn focus_request_remains_protected(
    grabbed: bool,
    ordinary_grab: Option<OrdinaryImplicitGrab>,
    requested: Option<SurfaceId>,
) -> bool {
    let ordinary_click_activation = requested.is_some()
        && ordinary_grab.is_some_and(|grab| grab.owner.is_none() || grab.owner == requested);
    grabbed && !ordinary_click_activation
}

fn shell_owns_cursor(
    no_client_focus: bool,
    pointer_is_grabbed: bool,
    ordinary_grab: Option<OrdinaryImplicitGrab>,
) -> bool {
    no_client_focus
        && (!pointer_is_grabbed || ordinary_grab.is_some_and(|grab| grab.owner.is_none()))
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

// Required by Smithay's WlSurface pointer target; no constraints global is advertised yet.
impl PointerConstraintsHandler for ServerState {}

impl SeatHandler for ServerState {
    fn touch_down_grab(
        &mut self,
        start: smithay::input::touch::GrabStartData<Self>,
    ) -> impl smithay::input::touch::TouchGrab<Self> {
        super::touch_grab::ContactGrab::new(start)
    }
    type KeyboardFocus = KeyboardFocus;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut smithay::input::SeatState<Self> {
        &mut self.seat_state
    }

    fn bind_seat(
        &mut self,
        client: &smithay::reexports::wayland_server::Client,
        seat: Seat<Self>,
    ) -> Seat<Self> {
        self.bind_input_seat(client, &seat).unwrap_or(seat)
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&KeyboardFocus>) {
        let Some(input_seat) = self.input_for_native(seat) else {
            return;
        };
        input_seat.keyboard_repeats.borrow_mut().focus_changed();
        input_seat.keyboard_diagnostic_dirty.set(true);
        let client = focused
            .and_then(|focus| focus.wl_surface())
            .and_then(|surface| self.display_handle.get_client(surface.id()).ok());
        let selection_seat = self.data_device_seat(seat);
        selection_seat
            .user_data()
            .insert_if_missing(|| super::seat_bindings::SelectionFocus(RefCell::default()));
        if let Some(owner) = selection_seat
            .user_data()
            .get::<super::seat_bindings::SelectionFocus>()
        {
            *owner.0.borrow_mut() = Some(seat.downgrade());
        }
        set_data_device_focus(&self.display_handle, &selection_seat, client);
    }

    fn cursor_image(&mut self, seat: &Seat<Self>, image: CursorImageStatus) {
        if let Some(input) = self.input_for_native(seat) {
            self.set_client_cursor_image(&input, image);
        }
    }
}

impl smithay::input::tablet::TabletSeatHandler for ServerState {
    type ToolFocus = WlSurface;
}

impl SelectionHandler for ServerState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for ServerState {
    fn data_device_seat(&self, seat: &Seat<Self>) -> Seat<Self> {
        seat.user_data()
            .get::<super::seat_bindings::SelectionGroup>()
            .map(|group| group.0.borrow().clone())
            .unwrap_or_else(|| seat.clone())
    }

    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DndGrabHandler for ServerState {}
impl WaylandDndGrabHandler for ServerState {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        _icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: smithay::utils::Serial,
        grab_type: GrabType,
    ) {
        if grab_type == GrabType::Touch {
            source.cancel();
            return;
        }
        let Some(pointer) = seat.get_pointer() else {
            warn!("cancelled pointer drag because the seat has no pointer");
            source.cancel();
            return;
        };
        let Some(start_data) = pointer.grab_start_data() else {
            warn!("cancelled pointer drag without an active implicit grab");
            source.cancel();
            return;
        };
        pointer.set_grab(
            self,
            DnDGrab::new_pointer(&self.display_handle, start_data, source, seat),
            serial,
            Focus::Keep,
        );
    }
}

fn compositor_point(position: InputPosition) -> smithay::utils::Point<f64, Logical> {
    (position.x, position.y).into()
}

fn gesture_delta(delta: InputDelta) -> smithay::utils::Point<f64, Logical> {
    (delta.x, delta.y).into()
}

const fn smithay_key_state(state: KeyboardKeyState) -> KeyEvent {
    match state {
        KeyboardKeyState::Pressed => KeyEvent::Pressed,
        KeyboardKeyState::Released => KeyEvent::Released,
        KeyboardKeyState::Repeated => KeyEvent::Repeated,
    }
}

const fn smithay_button_state(state: ButtonState) -> SmithayButtonState {
    match state {
        ButtonState::Pressed => SmithayButtonState::Pressed,
        ButtonState::Released => SmithayButtonState::Released,
    }
}

fn smithay_axis_frame(axis: RawScrollFrame, time: u32) -> Option<AxisFrame> {
    if axis.horizontal == 0.0
        && axis.vertical == 0.0
        && axis.horizontal_v120.unwrap_or_default() == 0
        && axis.vertical_v120.unwrap_or_default() == 0
        && !axis.horizontal_stop
        && !axis.vertical_stop
    {
        return None;
    }
    let source = match axis.source {
        RawScrollSource::Wheel => AxisSource::Wheel,
        RawScrollSource::Finger => AxisSource::Finger,
        RawScrollSource::Continuous => AxisSource::Continuous,
    };
    let mut frame = AxisFrame::new(InputTime::from_millis(time)).source(source);
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
    if axis.horizontal_stop {
        frame = frame.stop(Axis::Horizontal);
    }
    if axis.vertical_stop {
        frame = frame.stop(Axis::Vertical);
    }
    Some(frame)
}

#[cfg(test)]
mod tests {
    use crate::{
        input::{RawScrollFrame, RawScrollPhase, RawScrollSource},
        surface::SurfaceId,
    };

    use super::{
        FocusTransition, OrdinaryImplicitGrab, focus_request_remains_protected, shell_owns_cursor,
        smithay_axis_frame, transition_pending_focus,
    };

    #[test]
    fn only_positive_ordinary_click_state_bypasses_grab_focus_protection() {
        let first = SurfaceId::for_test(1);
        let second = SurfaceId::for_test(2);

        assert!(focus_request_remains_protected(true, None, Some(first)));
        assert!(focus_request_remains_protected(true, None, None));
        assert!(!focus_request_remains_protected(false, None, Some(first)));

        let client_click = Some(OrdinaryImplicitGrab { owner: Some(first) });
        assert!(!focus_request_remains_protected(
            true,
            client_click,
            Some(first)
        ));
        assert!(focus_request_remains_protected(
            true,
            client_click,
            Some(second)
        ));
        assert!(focus_request_remains_protected(true, client_click, None));

        let shell_click = Some(OrdinaryImplicitGrab { owner: None });
        assert!(!focus_request_remains_protected(
            true,
            shell_click,
            Some(second)
        ));
        assert!(focus_request_remains_protected(true, shell_click, None));
    }

    #[test]
    fn shell_cursor_remains_owned_during_a_shell_implicit_grab() {
        let shell_grab = Some(OrdinaryImplicitGrab { owner: None });
        let client_grab = Some(OrdinaryImplicitGrab {
            owner: Some(SurfaceId::for_test(1)),
        });

        assert!(shell_owns_cursor(true, true, shell_grab));
        assert!(!shell_owns_cursor(true, true, client_grab));
        assert!(!shell_owns_cursor(false, true, shell_grab));
    }

    #[test]
    fn focus_requests_apply_immediately_without_a_grab() {
        let surface = SurfaceId::for_test(1);
        let mut pending = Some(None);

        assert_eq!(
            transition_pending_focus(&mut pending, false, FocusTransition::Request(Some(surface)),),
            Some(Some(surface)),
        );
        assert_eq!(pending, None);
    }

    #[test]
    fn grabbed_focus_requests_queue_with_last_request_winning() {
        let first = SurfaceId::for_test(1);
        let second = SurfaceId::for_test(2);
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
        let surface = SurfaceId::for_test(1);
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
        let mut pending = Some(Some(SurfaceId::for_test(1)));

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
                    phase: RawScrollPhase::Moved,
                    horizontal: 0.0,
                    vertical: 0.0,
                    horizontal_v120: Some(0),
                    vertical_v120: None,
                    horizontal_stop: false,
                    vertical_stop: false,
                },
                1,
            )
            .is_none()
        );
    }

    #[test]
    fn preserves_stop_only_finger_frames() {
        assert!(
            smithay_axis_frame(
                RawScrollFrame {
                    source: RawScrollSource::Finger,
                    phase: RawScrollPhase::Ended,
                    horizontal: 0.0,
                    vertical: 0.0,
                    horizontal_v120: None,
                    vertical_v120: None,
                    horizontal_stop: false,
                    vertical_stop: true,
                },
                1,
            )
            .is_some()
        );
    }
}
