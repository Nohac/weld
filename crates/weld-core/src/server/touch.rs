//! Controller-scoped native contacts and primary-contact pointer fallback.
use super::{ServerState, input_seat::InputSeat, seat_bindings::Device};
use crate::{input::SurfaceHit, surface::SurfaceId};
use smithay::{
    backend::input::InputTime,
    input::touch::{DownEvent, MotionEvent, UpEvent},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::SERIAL_COUNTER,
};
use std::{collections::HashMap, rc::Rc};
use weld_client::{
    ButtonState, ClientInputController, ClientInputEvent, ClientInputTarget, InputEventKind,
    InputPosition, LinuxButtonCode, MAX_TOUCH_CONTACTS, TouchEvent, TouchId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TouchMode {
    Native,
    Pointer,
}

#[derive(Clone)]
struct Contact {
    input: Rc<InputSeat>,
    target: ClientInputTarget,
    native_surface: WlSurface,
    slot: u32,
    position: InputPosition,
}

#[derive(Default)]
pub(super) struct TouchRouting {
    contacts: HashMap<(Option<ClientInputController>, TouchId), Contact>,
    pending: Vec<(
        Option<ClientInputController>,
        ClientInputTarget,
        Rc<InputSeat>,
    )>,
}

impl ServerState {
    pub(super) fn route_touch(
        &mut self,
        controller: Option<ClientInputController>,
        target: ClientInputTarget,
        event: TouchEvent,
        time: u32,
    ) {
        let ClientInputTarget::Touch { surface, layer } = target else {
            return;
        };
        if !event.is_valid()
            || controller.is_some_and(|owner| !self.remote_inputs.contains_key(&owner))
        {
            return;
        }
        match event {
            TouchEvent::Down { id, position } => {
                if self.touch.contacts.contains_key(&(controller, id))
                    || self
                        .touch
                        .contacts
                        .keys()
                        .filter(|(owner, _)| *owner == controller)
                        .count()
                        >= MAX_TOUCH_CONTACTS
                    || self
                        .touch
                        .pending
                        .iter()
                        .filter(|(owner, _, _)| *owner == controller)
                        .count()
                        >= MAX_TOUCH_CONTACTS
                {
                    return;
                }
                let Some(focus) = self.pointer_focus(
                    position,
                    Some(SurfaceHit {
                        surface,
                        layer,
                        local_position: position,
                    }),
                ) else {
                    return;
                };
                let client = self.input_client(surface);
                let pinned = self
                    .touch
                    .contacts
                    .iter()
                    .filter(|((owner, _), _)| *owner == controller)
                    .map(|(_, contact)| (contact.target, &contact.input))
                    .chain(
                        self.touch
                            .pending
                            .iter()
                            .filter(|(owner, _, _)| *owner == controller)
                            .map(|(_, target, input)| (*target, input)),
                    )
                    .find(|(target, _)| self.input_client(target.surface()) == client)
                    .map(|(_, input)| input.clone());
                let Some(input) = pinned
                    .or_else(|| self.select_binding(controller, surface, Device::Touch, true))
                    .or_else(|| self.select_binding(controller, surface, Device::Pointer, true))
                else {
                    return;
                };
                let mode = match input.touch_mode.get() {
                    Some(mode) => mode,
                    None => {
                        let native = self.input_client(surface).is_some_and(|client| {
                            input
                                .native
                                .get_touch()
                                .is_some_and(|touch| touch.client_touch(&client).next().is_some())
                        });
                        if !native
                            && (!input.pressed_pointer_buttons.borrow().is_empty()
                                || input
                                    .native
                                    .get_pointer()
                                    .is_some_and(|pointer| pointer.is_grabbed()))
                        {
                            return;
                        }
                        let mode = if native {
                            TouchMode::Native
                        } else {
                            TouchMode::Pointer
                        };
                        tracing::debug!(target: "weld_input_diag", ?controller, ?surface, ?mode, "selected touch delivery");
                        input.touch_mode.set(Some(mode));
                        input.touch_primary.set(Some(id));
                        mode
                    }
                };
                let Some(slot) = (0..MAX_TOUCH_CONTACTS)
                    .filter_map(|slot| u32::try_from(slot).ok())
                    .find(|slot| {
                        !self.touch.contacts.values().any(|contact| {
                            Rc::ptr_eq(&contact.input, &input) && contact.slot == *slot
                        })
                    })
                else {
                    return;
                };
                if input.touch_count.get() == 0 {
                    input.touch_primary.set(Some(id));
                }
                input.touch_count.set(input.touch_count.get() + 1);
                self.touch.contacts.insert(
                    (controller, id),
                    Contact {
                        input: input.clone(),
                        target,
                        native_surface: focus.0.clone(),
                        slot,
                        position,
                    },
                );
                self.mark_touch_frame(controller, target, &input);
                match mode {
                    TouchMode::Native => {
                        if let Some(touch) = input.native.get_touch() {
                            touch.down(
                                self,
                                Some(focus),
                                &DownEvent {
                                    slot: Some(slot).into(),
                                    location: (position.x, position.y).into(),
                                    serial: SERIAL_COUNTER.next_serial(),
                                    time: InputTime::from_millis(time),
                                },
                            );
                        }
                    }
                    TouchMode::Pointer if input.touch_primary.get() == Some(id) => {
                        self.deliver_touch_pointer(
                            &input,
                            target,
                            position,
                            Some(ButtonState::Pressed),
                            time,
                        );
                    }
                    TouchMode::Pointer => {}
                }
            }
            TouchEvent::Motion { id, position } => {
                let Some(contact) = self
                    .touch
                    .contacts
                    .get_mut(&(controller, id))
                    .filter(|contact| contact.target == target)
                else {
                    return;
                };
                contact.position = position;
                let contact = contact.clone();
                self.mark_touch_frame(controller, target, &contact.input);
                match contact.input.touch_mode.get() {
                    Some(TouchMode::Native) => {
                        if let Some(touch) = contact.input.native.get_touch() {
                            touch.motion(
                                self,
                                None,
                                &MotionEvent {
                                    slot: Some(contact.slot).into(),
                                    location: (position.x, position.y).into(),
                                    time: InputTime::from_millis(time),
                                },
                            );
                        }
                    }
                    Some(TouchMode::Pointer) if contact.input.touch_primary.get() == Some(id) => {
                        self.deliver_touch_pointer(&contact.input, target, position, None, time);
                    }
                    _ => {}
                }
            }
            TouchEvent::Up { id } => {
                if !self
                    .touch
                    .contacts
                    .get(&(controller, id))
                    .is_some_and(|contact| contact.target == target)
                {
                    return;
                }
                let Some(contact) = self.touch.contacts.remove(&(controller, id)) else {
                    return;
                };
                contact
                    .input
                    .touch_count
                    .set(contact.input.touch_count.get().saturating_sub(1));
                self.mark_touch_frame(controller, target, &contact.input);
                match contact.input.touch_mode.get() {
                    Some(TouchMode::Native) => {
                        if let Some(touch) = contact.input.native.get_touch() {
                            touch.up(
                                self,
                                &UpEvent {
                                    slot: Some(contact.slot).into(),
                                    serial: SERIAL_COUNTER.next_serial(),
                                    time: InputTime::from_millis(time),
                                },
                            );
                        }
                    }
                    Some(TouchMode::Pointer) if contact.input.touch_primary.get() == Some(id) => {
                        self.deliver_touch_pointer(
                            &contact.input,
                            target,
                            contact.position,
                            Some(ButtonState::Released),
                            time,
                        );
                        contact.input.touch_primary.set(None);
                        self.clear_seat_pointer(&contact.input, time);
                    }
                    _ => {}
                }
            }
            TouchEvent::Frame | TouchEvent::Cancel => {
                let input = self
                    .touch
                    .pending
                    .iter()
                    .find(|(owner, current, _)| *owner == controller && *current == target)
                    .map(|(_, _, input)| input.clone())
                    .or_else(|| {
                        self.touch
                            .contacts
                            .iter()
                            .find(|((owner, _), contact)| {
                                *owner == controller && contact.target == target
                            })
                            .map(|(_, contact)| contact.input.clone())
                    });
                let Some(input) = input else {
                    return;
                };
                if event == TouchEvent::Cancel {
                    self.cancel_touch_context(&input, time);
                } else {
                    self.finish_touch_frame(&input);
                }
            }
        }
    }

    fn mark_touch_frame(
        &mut self,
        controller: Option<ClientInputController>,
        target: ClientInputTarget,
        input: &Rc<InputSeat>,
    ) {
        input.touch_pending.set(true);
        if !self
            .touch
            .pending
            .iter()
            .any(|(owner, current, _)| *owner == controller && *current == target)
        {
            self.touch.pending.push((controller, target, input.clone()));
        }
    }

    fn finish_touch_frame(&mut self, input: &InputSeat) {
        self.touch
            .pending
            .retain(|(_, _, current)| current.native != input.native);
        if input.touch_pending.replace(false)
            && input.touch_mode.get() == Some(TouchMode::Native)
            && let Some(touch) = input.native.get_touch()
        {
            touch.frame(self);
        }
        if input.touch_count.get() == 0 {
            input.touch_mode.set(None);
            input.touch_primary.set(None);
        }
    }

    pub(super) fn cancel_touch_context(&mut self, input: &InputSeat, time: u32) {
        let mode = input.touch_mode.get();
        if mode.is_none() {
            return;
        }
        let primary = input
            .touch_primary
            .get()
            .and_then(|id| self.touch.contacts.get(&(input.controller.get(), id)))
            .cloned();
        if mode == Some(TouchMode::Native)
            && let Some(touch) = input.native.get_touch()
        {
            if input.touch_count.get() > 0 {
                touch.cancel(self);
            } else if input.touch_pending.get() {
                touch.frame(self);
            }
        } else if let Some(primary) = primary {
            self.deliver_touch_pointer(
                input,
                primary.target,
                primary.position,
                Some(ButtonState::Released),
                time,
            );
            self.clear_seat_pointer(input, time);
        }
        self.touch
            .contacts
            .retain(|_, contact| contact.input.native != input.native);
        self.touch
            .pending
            .retain(|(_, _, current)| current.native != input.native);
        input.touch_count.set(0);
        input.touch_pending.set(false);
        input.touch_primary.set(None);
        input.touch_mode.set(None);
    }

    pub(super) fn retire_touch_target(&mut self, surface: SurfaceId, time: u32) {
        let mut inputs = Vec::new();
        for (_, target, input) in &self.touch.pending {
            if target.surface() == surface
                && !inputs.iter().any(|current| Rc::ptr_eq(current, input))
            {
                inputs.push(input.clone());
            }
        }
        for contact in self.touch.contacts.values() {
            if contact.target.surface() == surface
                && !inputs.iter().any(|input| Rc::ptr_eq(input, &contact.input))
            {
                inputs.push(contact.input.clone());
            }
        }
        for input in inputs {
            self.cancel_touch_context(&input, time);
        }
    }

    pub(super) fn retire_touch_surface(&mut self, surface: &WlSurface, time: u32) {
        let mut inputs = Vec::new();
        for contact in self.touch.contacts.values() {
            if contact.native_surface == *surface
                && !inputs.iter().any(|input| Rc::ptr_eq(input, &contact.input))
            {
                inputs.push(contact.input.clone());
            }
        }
        for input in inputs {
            self.cancel_touch_context(&input, time);
        }
    }

    fn deliver_touch_pointer(
        &mut self,
        input: &InputSeat,
        target: ClientInputTarget,
        position: InputPosition,
        button: Option<ButtonState>,
        time: u32,
    ) {
        let ClientInputTarget::Touch { surface, layer } = target else {
            return;
        };
        let logical = self.controller_seat(input.controller.get());
        if let Some(logical) = &logical
            && let Some(binding) = self.input_for_native(&input.native)
        {
            self.track_pointer_binding(logical, binding, time);
        }
        let event = button.map_or(
            InputEventKind::PointerMotion {
                position,
                relative: None,
            },
            |state| InputEventKind::PointerButton {
                position: Some(position),
                button: LinuxButtonCode(0x110),
                state,
            },
        );
        self.deliver_client_input(
            input,
            ClientInputEvent {
                target: ClientInputTarget::Pointer { surface, layer },
                host_position: None,
                event,
                time,
            },
        );
        if let Some(logical) = logical {
            logical.shell_owns_cursor.set(input.shell_owns_cursor.get());
        }
    }
}
