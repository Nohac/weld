//! Independent native input state for the desktop and each remote controller.

use super::{ServerState, seat::OrdinaryImplicitGrab};
use crate::{
    input::{InputPosition, KeyboardRepeatTracker},
    surface::SurfaceId,
};
use smithay::{
    desktop::PopupGrab,
    input::{Seat, pointer::CursorImageStatus},
};
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    rc::Rc,
};
use weld_client::{ClientFocusRequest, ClientInputEvent};

// Smithay delivery re-enters ServerState through focus/cursor callbacks. Shared
// handles keep the selected seat available to those callbacks; cell borrows
// finish before calling Smithay.
pub(super) struct InputSeat {
    pub native: Seat<ServerState>,
    pub touch_mode: Cell<Option<super::touch::TouchMode>>,
    pub touch_count: Cell<usize>,
    pub touch_pending: Cell<bool>,
    pub touch_primary: Cell<Option<weld_client::TouchId>>,
    pub controller: Cell<Option<weld_client::ClientInputController>>,
    pub admission: RefCell<super::input_admission::InputAdmission>,
    pub active_gesture: Cell<Option<weld_client::PointerGestureKind>>,
    pub finger_scroll: Cell<bool>,
    pub desired_focus: Cell<Option<SurfaceId>>,
    pub keyboard_binding: RefCell<Option<Rc<InputSeat>>>,
    pub pointer_binding: RefCell<Option<Rc<InputSeat>>>,
    pub focused_toplevel: Cell<Option<SurfaceId>>,
    pub pending_focus: RefCell<Option<Option<SurfaceId>>>,
    pub pointer_position: Cell<InputPosition>,
    pub ordinary_implicit_grab: Cell<Option<OrdinaryImplicitGrab>>,
    pub pressed_pointer_buttons: RefCell<HashSet<u32>>,
    pub keyboard_repeats: RefCell<KeyboardRepeatTracker>,
    pub keyboard_diagnostic_dirty: Cell<bool>,
    pub popup_grab: RefCell<Option<PopupGrab<ServerState>>>,
    pub cursor_status: RefCell<CursorImageStatus>,
    pub cursor_feedback_dirty: Cell<bool>,
    pub shell_owns_cursor: Cell<bool>,
}

impl InputSeat {
    pub fn new(
        native: Seat<ServerState>,
        controller: Option<weld_client::ClientInputController>,
    ) -> Rc<Self> {
        Rc::new(Self {
            native,
            touch_mode: Cell::new(None),
            touch_count: Cell::new(0),
            touch_pending: Cell::new(false),
            touch_primary: Cell::new(None),
            controller: Cell::new(controller),
            admission: RefCell::default(),
            active_gesture: Cell::new(None),
            finger_scroll: Cell::new(false),
            desired_focus: Cell::new(None),
            keyboard_binding: RefCell::default(),
            pointer_binding: RefCell::default(),
            focused_toplevel: Cell::new(None),
            pending_focus: RefCell::new(None),
            pointer_position: Cell::new(InputPosition::default()),
            ordinary_implicit_grab: Cell::new(None),
            pressed_pointer_buttons: RefCell::default(),
            keyboard_repeats: RefCell::default(),
            keyboard_diagnostic_dirty: Cell::new(true),
            popup_grab: RefCell::default(),
            cursor_status: RefCell::new(CursorImageStatus::default_named()),
            cursor_feedback_dirty: Cell::new(true),
            shell_owns_cursor: Cell::new(true),
        })
    }
}

impl ServerState {
    pub(super) fn input_seats(&self) -> impl Iterator<Item = &Rc<InputSeat>> {
        std::iter::once(&self.local_input)
            .chain(self.remote_inputs.values())
            .chain(self.input_bindings.iter().map(|binding| &binding.input))
    }

    pub(super) fn input_for_native(&self, seat: &Seat<Self>) -> Option<Rc<InputSeat>> {
        self.input_seats()
            .find(|input| input.native == *seat)
            .cloned()
    }

    pub(super) fn remote_input(
        &mut self,
        controller: weld_client::ClientInputController,
    ) -> Option<Rc<InputSeat>> {
        if let Some(input) = self.remote_inputs.get(&controller) {
            return Some(input.clone());
        }
        let mut native = self.seat_state.new_wl_seat(
            &self.display_handle,
            format!(
                "weld-peer-{}-{}",
                controller.adapter.raw(),
                controller.connection
            ),
        );
        let keyboard = match native.add_keyboard(Default::default(), 200, 25) {
            Ok(keyboard) => keyboard,
            Err(error) => {
                if let Some(global) = native.global() {
                    self.display_handle.remove_global::<Self>(global);
                }
                self.seat_state.remove_seat(&native);
                tracing::warn!(%error, "could not create remote keyboard");
                return None;
            }
        };
        if let Err(error) =
            keyboard.set_keymap_from_string(self, self.keyboard_mapper.keymap().as_str().to_owned())
        {
            if let Some(global) = native.global() {
                self.display_handle.remove_global::<Self>(global);
            }
            native.remove_keyboard();
            self.seat_state.remove_seat(&native);
            tracing::warn!(%error, "could not configure remote keyboard");
            return None;
        }
        native.add_pointer();
        native.add_touch();
        let input = InputSeat::new(native, Some(controller));
        self.remote_inputs.insert(controller, input.clone());
        self.configure_seat_repeat(&input);
        Some(input)
    }

    pub(super) fn remote_client_input(
        &mut self,
        controller: weld_client::ClientInputController,
        event: ClientInputEvent,
    ) {
        self.route_controller_input(Some(controller), event);
    }

    pub(super) fn remote_client_focus(
        &mut self,
        controller: weld_client::ClientInputController,
        focus: ClientFocusRequest,
    ) {
        self.route_controller_focus(Some(controller), focus.surface);
    }

    pub(super) fn retire_remote_seat(&mut self, controller: weld_client::ClientInputController) {
        if let Some(input) = self.remote_inputs.get(&controller).cloned() {
            self.release_controller_input(Some(controller), self.event_time());
            self.retire_input_bindings(controller);
            smithay::wayland::selection::data_device::clear_data_device_selection(
                &self.display_handle,
                &input.native,
            );
            let mut native = input.native.clone();
            native.remove_pointer();
            native.remove_touch();
            native.remove_keyboard();
            if let Some(global) = input.native.global() {
                self.display_handle.remove_global::<Self>(global);
            }
            self.remote_inputs.remove(&controller);
            self.seat_state.remove_seat(&native);
        }
    }
}
