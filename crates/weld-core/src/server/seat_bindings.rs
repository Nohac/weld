//! Application-visible seat bindings and controller arbitration.
use super::{ServerState, input_seat::InputSeat};
use crate::surface::SurfaceId;
use smithay::wayland::selection::data_device::move_data_devices;
use smithay::{
    input::Seat,
    reexports::wayland_server::{Client, Resource},
};
use std::{cell::RefCell, rc::Rc};
use weld_client::{ClientInputController, ClientInputEvent, InputEventKind};

pub(super) struct InputBinding {
    pub client: Client,
    pub published: Option<ClientInputController>,
    pub input: Rc<InputSeat>,
}

pub(super) struct SelectionGroup(pub RefCell<Seat<ServerState>>);
pub(super) struct SelectionFocus(pub RefCell<Option<smithay::input::WeakSeat<ServerState>>>);

#[derive(Clone, Copy)]
pub(super) enum Device {
    Touch,
    Keyboard,
    Pointer,
}

impl InputSeat {
    pub(super) fn busy(&self) -> bool {
        self.native.get_keyboard().is_some_and(|keyboard| {
            self.keyboard_repeats.borrow().has_pressed_keys() || keyboard.is_grabbed()
        }) || self
            .native
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed())
            || !self.pressed_pointer_buttons.borrow().is_empty()
            || self.active_gesture.get().is_some()
            || self.finger_scroll.get()
            || self.touch_count.get() != 0
            || self.touch_pending.get()
    }

    fn bound(&self, client: &Client, device: Device) -> bool {
        match device {
            Device::Touch => self
                .native
                .get_touch()
                .is_some_and(|touch| touch.client_touch(client).next().is_some()),
            Device::Keyboard => self
                .native
                .get_keyboard()
                .is_some_and(|keyboard| keyboard.client_keyboards(client).next().is_some()),
            Device::Pointer => self
                .native
                .get_pointer()
                .is_some_and(|pointer| pointer.client_pointers(client).next().is_some()),
        }
    }
}

impl ServerState {
    fn move_selection_group(input: &Seat<Self>, next: Seat<Self>) {
        if let Some(group) = input.user_data().get::<SelectionGroup>() {
            move_data_devices(input, &group.0.borrow(), &next);
            *group.0.borrow_mut() = next;
        }
    }

    pub(super) fn retire_input_bindings(&mut self, controller: ClientInputController) {
        let mut index = 0;
        while index < self.input_bindings.len() {
            let binding = &self.input_bindings[index];
            if binding.published == Some(controller) {
                let binding = self.input_bindings.remove(index);
                let mut native = binding.input.native.clone();
                native.remove_pointer();
                native.remove_touch();
                native.remove_keyboard();
                self.seat_state.remove_seat(&native);
            } else {
                if binding.input.controller.get() == Some(controller) {
                    let input = binding.input.clone();
                    Self::move_selection_group(&input.native, self.local_input.native.clone());
                    input.controller.set(None);
                    self.configure_seat_repeat(&input);
                }
                index += 1;
            }
        }
    }

    pub(super) fn cleanup_input_bindings(&mut self) {
        let backend = self.display_handle.backend_handle();
        let mut index = 0;
        while index < self.input_bindings.len() {
            if backend
                .get_client_data(self.input_bindings[index].client.id())
                .is_ok()
            {
                index += 1;
                continue;
            }
            let input = self.input_bindings[index].input.clone();
            self.release_seat_input(&input, self.event_time());
            self.focus_seat(&input, None);
            for logical in std::iter::once(&self.local_input).chain(self.remote_inputs.values()) {
                for route in [&logical.keyboard_binding, &logical.pointer_binding] {
                    if route
                        .borrow()
                        .as_ref()
                        .is_some_and(|held| Rc::ptr_eq(held, &input))
                    {
                        route.borrow_mut().take();
                    }
                }
            }
            let mut native = input.native.clone();
            native.remove_pointer();
            native.remove_touch();
            native.remove_keyboard();
            self.seat_state.remove_seat(&native);
            self.input_bindings.remove(index);
        }
    }
    pub(super) fn bind_input_seat(
        &mut self,
        client: &Client,
        global: &Seat<Self>,
    ) -> Option<Seat<Self>> {
        let controller = self.input_for_native(global)?.controller.get();
        if let Some(binding) = self
            .input_bindings
            .iter()
            .find(|binding| binding.client == *client && binding.published == controller)
        {
            return Some(binding.input.native.clone());
        }
        let mut native = self.seat_state.new_seat(global.name());
        let keyboard = match native.add_keyboard(Default::default(), 200, 25) {
            Ok(keyboard) => keyboard,
            Err(error) => {
                tracing::warn!(%error, "could not create application input context");
                self.seat_state.remove_seat(&native);
                return None;
            }
        };
        if let Err(error) =
            keyboard.set_keymap_from_string(self, self.keyboard_mapper.keymap().as_str().to_owned())
        {
            tracing::warn!(%error, "could not configure application keyboard");
            native.remove_keyboard();
            self.seat_state.remove_seat(&native);
            return None;
        }
        native.add_pointer();
        native.add_touch();
        native
            .user_data()
            .insert_if_missing(|| SelectionGroup(RefCell::new(global.clone())));
        let input = InputSeat::new(native.clone(), controller);
        self.input_bindings.push(InputBinding {
            client: client.clone(),
            published: controller,
            input: input.clone(),
        });
        self.configure_seat_repeat(&input);
        Some(native)
    }

    pub(super) fn controller_seat(
        &self,
        controller: Option<ClientInputController>,
    ) -> Option<Rc<InputSeat>> {
        match controller {
            None => Some(self.local_input.clone()),
            Some(controller) => self.remote_inputs.get(&controller).cloned(),
        }
    }

    pub(super) fn input_client(&self, surface: SurfaceId) -> Option<Client> {
        self.toplevels
            .get(surface)
            .map(|window| window.surface.wl_surface())
            .or_else(|| {
                self.popups
                    .get(surface)
                    .map(|popup| popup.surface.wl_surface())
            })
            .or_else(|| {
                self.layers
                    .0
                    .get(surface)
                    .map(|layer| layer.surface.wl_surface())
            })?
            .client()
    }

    pub(super) fn select_binding(
        &mut self,
        controller: Option<ClientInputController>,
        surface: SurfaceId,
        device: Device,
        takeover: bool,
    ) -> Option<Rc<InputSeat>> {
        let client = self.input_client(surface)?;
        // Keep a sequence on its admitted devices even if native bindings arrive later.
        let pinned = self.input_bindings.iter().find(|binding| {
            binding.client == client
                && binding.input.controller.get() == controller
                && binding.input.busy()
                && binding.input.bound(&client, device)
        });
        let candidate = pinned
            .or_else(|| {
                self.input_bindings.iter().find(|binding| {
                    binding.client == client
                        && binding.published == controller
                        && controller.is_some()
                        && if matches!(device, Device::Touch) {
                            binding.input.bound(&client, Device::Touch)
                        } else {
                            binding.input.bound(&client, Device::Keyboard)
                                && binding.input.bound(&client, Device::Pointer)
                        }
                })
            })
            .or_else(|| {
                self.input_bindings.iter().find(|binding| {
                    binding.client == client
                        && binding.published.is_none()
                        && binding.input.bound(&client, device)
                })
            })?;
        let input = candidate.input.clone();
        if input.controller.get() != controller {
            if !takeover {
                return None;
            }
            if input.busy() {
                tracing::debug!(
                    ?controller,
                    ?surface,
                    "application input context is held by another controller"
                );
                return None;
            }
            self.release_seat_input(&input, self.event_time());
            self.focus_seat(&input, None);
            let next = self.controller_seat(controller)?.native.clone();
            Self::move_selection_group(&input.native, next);
            input.controller.set(controller);
            self.configure_seat_repeat(&input);
        }
        Some(input)
    }

    pub(super) fn track_pointer_binding(
        &mut self,
        logical: &InputSeat,
        input: Rc<InputSeat>,
        time: u32,
    ) {
        let previous = logical.pointer_binding.borrow_mut().replace(input.clone());
        if let Some(previous) = previous
            && previous.controller.get() == logical.controller.get()
            && !Rc::ptr_eq(&previous, &input)
        {
            self.clear_seat_pointer(&previous, time);
        }
    }

    pub(super) fn route_controller_focus(
        &mut self,
        controller: Option<ClientInputController>,
        surface: Option<SurfaceId>,
    ) {
        let Some(logical) = self.controller_seat(controller) else {
            return;
        };
        logical.desired_focus.set(surface);
        let next = surface
            .and_then(|surface| self.select_binding(controller, surface, Device::Keyboard, true));
        let previous = logical.keyboard_binding.borrow_mut().take();
        if let Some(previous) = previous
            && previous.controller.get() == controller
            && !next
                .as_ref()
                .is_some_and(|next| Rc::ptr_eq(next, &previous))
        {
            self.focus_seat(&previous, None);
        }
        if let Some(next) = &next {
            self.focus_seat(next, surface);
        }
        *logical.keyboard_binding.borrow_mut() = next;
    }

    pub(super) fn route_controller_input(
        &mut self,
        controller: Option<ClientInputController>,
        event: ClientInputEvent,
    ) {
        if let InputEventKind::Touch { event: touch } = event.event {
            self.route_touch(controller, event.target, touch, event.time);
            return;
        }
        let Some(logical) = self.controller_seat(controller) else {
            return;
        };
        if !logical.admission.borrow().allows(&event.event) {
            logical.admission.borrow_mut().observe(&event, false);
            return;
        }
        if matches!(event.event, InputEventKind::Keyboard { keycode, .. } if keycode.0.checked_add(8).is_none())
        {
            return;
        }
        let keyboard = matches!(event.event, InputEventKind::Keyboard { .. });
        let device = if keyboard {
            Device::Keyboard
        } else {
            Device::Pointer
        };
        let Some(input) = self.select_binding(
            controller,
            event.target.surface(),
            device,
            super::input_admission::starts_interaction(&event.event),
        ) else {
            logical.admission.borrow_mut().observe(&event, false);
            return;
        };
        let restore_keyboard = logical
            .keyboard_binding
            .borrow()
            .as_ref()
            .is_none_or(|current| {
                current.controller.get() != controller
                    || !Rc::ptr_eq(current, &input)
                    || current
                        .native
                        .get_keyboard()
                        .and_then(|keyboard| keyboard.current_focus())
                        .is_none()
            });
        let desired = logical.desired_focus.get();
        if keyboard {
            if restore_keyboard && desired == Some(event.target.surface()) {
                self.route_controller_focus(controller, desired);
            }
        } else {
            self.track_pointer_binding(&logical, input.clone(), event.time);
            if restore_keyboard
                && let Some(focus) = desired
                && self.input_client(focus) == self.input_client(event.target.surface())
            {
                self.route_controller_focus(controller, desired);
            }
        }
        logical.admission.borrow_mut().observe(&event, true);
        self.deliver_client_input(&input, event);
        if !keyboard {
            logical.shell_owns_cursor.set(input.shell_owns_cursor.get());
        }
    }

    pub(super) fn release_controller_input(
        &mut self,
        controller: Option<ClientInputController>,
        time: u32,
    ) {
        let inputs = self
            .input_bindings
            .iter()
            .filter(|binding| binding.input.controller.get() == controller)
            .map(|binding| binding.input.clone())
            .collect::<Vec<_>>();
        for input in inputs {
            self.release_seat_input(&input, time);
            self.focus_seat(&input, None);
        }
        if let Some(logical) = self.controller_seat(controller) {
            logical.admission.borrow_mut().clear();
            logical.desired_focus.set(None);
            logical.keyboard_binding.borrow_mut().take();
            logical.pointer_binding.borrow_mut().take();
        }
    }
}
