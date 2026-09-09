use std::{
    fmt,
    sync::{Arc, Mutex},
};

use wayland_protocols_misc::zwp_input_method_v2::server::zwp_input_method_keyboard_grab_v2::{
    self, ZwpInputMethodKeyboardGrabV2,
};
use wayland_server::backend::ClientId;

use crate::input::{
    SeatHandler,
    keyboard::{
        GrabStartData as KeyboardGrabStartData, KeyboardGrab, KeyboardHandle, KeyboardInnerHandle,
        ModifiersState,
    },
};
use crate::wayland::text_input::TextInputHandle;
use crate::{
    backend::input::{InputTime, KeyEvent, Keycode},
    utils::Serial,
    wayland::Dispatch2,
};

#[derive(Default, Debug)]
pub(crate) struct InputMethodKeyboard {
    pub grab: Option<ZwpInputMethodKeyboardGrabV2>,
    pub text_input_handle: TextInputHandle,
}

/// Handle to an input method instance
#[derive(Default, Debug, Clone)]
pub struct InputMethodKeyboardGrab {
    pub(crate) inner: Arc<Mutex<InputMethodKeyboard>>,
}

impl<D> KeyboardGrab<D> for InputMethodKeyboardGrab
where
    D: SeatHandler + 'static,
{
    fn input(
        &mut self,
        _data: &mut D,
        handle: &mut KeyboardInnerHandle<'_, D>,
        keycode: Keycode,
        key_state: KeyEvent,
        modifiers: Option<ModifiersState>,
        serial: Serial,
        time: InputTime,
    ) {
        // This protocol has repeat_info but no state 2. Only the explicit
        // emulation policy replaces a repeat with wire edges, never XKB updates.
        if key_state == KeyEvent::Repeated && !handle.emulate_legacy_repeats() {
            return;
        }
        let inner = self.inner.lock().unwrap();
        let keyboard = inner.grab.as_ref().unwrap();
        inner
            .text_input_handle
            .active_text_input_serial_or_default(serial.0, |serial| {
                if key_state == KeyEvent::Repeated {
                    keyboard.key(
                        serial,
                        time.millis(),
                        keycode.raw() - 8,
                        KeyEvent::Released.into(),
                    );
                    keyboard.key(serial, time.millis(), keycode.raw() - 8, KeyEvent::Pressed.into());
                } else {
                    keyboard.key(serial, time.millis(), keycode.raw() - 8, key_state.into());
                }
                if let Some(serialized) = modifiers.map(|m| m.serialized) {
                    keyboard.modifiers(
                        serial,
                        serialized.depressed,
                        serialized.latched,
                        serialized.locked,
                        serialized.layout_effective,
                    )
                }
            });
    }

    fn set_focus(
        &mut self,
        data: &mut D,
        handle: &mut KeyboardInnerHandle<'_, D>,
        focus: Option<<D as SeatHandler>::KeyboardFocus>,
        serial: crate::utils::Serial,
    ) {
        handle.set_focus(data, focus, serial)
    }

    fn start_data(&self) -> &KeyboardGrabStartData<D> {
        &KeyboardGrabStartData { focus: None }
    }

    fn unset(&mut self, _data: &mut D) {}
}

/// User data of ZwpInputKeyboardGrabV2 object
pub struct InputMethodKeyboardUserData<D: SeatHandler> {
    pub(super) handle: InputMethodKeyboardGrab,
    pub(crate) keyboard_handle: KeyboardHandle<D>,
}

impl<D: SeatHandler> fmt::Debug for InputMethodKeyboardUserData<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputMethodKeyboardUserData")
            .field("handle", &self.handle)
            .field("keyboard_handle", &self.keyboard_handle)
            .finish()
    }
}

impl<D: SeatHandler + 'static> Dispatch2<ZwpInputMethodKeyboardGrabV2, D> for InputMethodKeyboardUserData<D> {
    fn destroyed(&self, state: &mut D, _client: ClientId, _object: &ZwpInputMethodKeyboardGrabV2) {
        self.handle.inner.lock().unwrap().grab = None;
        self.keyboard_handle.unset_grab(state);
    }

    fn request(
        &self,
        _state: &mut D,
        _client: &wayland_server::Client,
        _resource: &ZwpInputMethodKeyboardGrabV2,
        request: zwp_input_method_keyboard_grab_v2::Request,
        _dhandle: &wayland_server::DisplayHandle,
        _data_init: &mut wayland_server::DataInit<'_, D>,
    ) {
        match request {
            zwp_input_method_keyboard_grab_v2::Request::Release => {
                // Nothing to do
            }
            _ => unreachable!(),
        }
    }
}
