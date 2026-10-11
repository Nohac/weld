//! Pointer delivery confined to the locker's output surfaces.

use smithay::{
    backend::input::{ButtonState as NativeButtonState, InputTime},
    input::pointer::{ButtonEvent, MotionEvent},
    reexports::wayland_server::Resource,
    utils::SERIAL_COUNTER,
};

use crate::{
    input::{ButtonState, RawSeatEvent, RawSeatEventKind},
    server::ServerState,
};

impl ServerState {
    pub(super) fn dispatch_lock_pointer(&mut self, event: &RawSeatEvent) {
        let position = match event.event {
            RawSeatEventKind::PointerMotion { position, .. }
            | RawSeatEventKind::PointerLeft { position } => Some(position),
            RawSeatEventKind::PointerButton { position, .. }
            | RawSeatEventKind::PointerAxis { position, .. } => position,
            _ => None,
        }
        .unwrap_or(self.local_input.pointer_position.get());
        self.local_input.pointer_position.set(position);
        let Some((content, origin)) = self.session_lock.outputs.iter().find_map(|(id, content)| {
            let output = self.outputs.get(id)?;
            let origin = output.logical_position;
            let size = output
                .metrics
                .mode()
                .size
                .to_f64()
                .to_logical(output.metrics.scale_factor());
            (content.surface.alive()
                && content.surface.ext_session_lock().is_alive()
                && position.x >= f64::from(origin.0)
                && position.y >= f64::from(origin.1)
                && position.x < f64::from(origin.0) + size.w
                && position.y < f64::from(origin.1) + size.h)
                .then(|| (content.surface.wl_surface().clone(), origin))
        }) else {
            return;
        };
        let Some(client) = content.client() else {
            return;
        };
        let global = self.local_input.native.clone();
        let Some(seat) = self.bind_input_seat(&client, &global) else {
            return;
        };
        let Some(input) = self.input_for_native(&seat) else {
            return;
        };
        let Some(pointer) = seat.get_pointer() else {
            return;
        };
        let focus = (!matches!(event.event, RawSeatEventKind::PointerLeft { .. }))
            .then(|| (content, (f64::from(origin.0), f64::from(origin.1)).into()));
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: (position.x, position.y).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::from_millis(event.time),
            },
        );
        match event.event {
            RawSeatEventKind::PointerButton { button, state, .. } => {
                let send = match state {
                    ButtonState::Pressed => {
                        input.pressed_pointer_buttons.borrow_mut().insert(button.0)
                    }
                    ButtonState::Released => {
                        input.pressed_pointer_buttons.borrow_mut().remove(&button.0)
                    }
                };
                if send {
                    pointer.button(
                        self,
                        &ButtonEvent {
                            button: button.0,
                            state: if state == ButtonState::Pressed {
                                NativeButtonState::Pressed
                            } else {
                                NativeButtonState::Released
                            },
                            serial: SERIAL_COUNTER.next_serial(),
                            time: InputTime::from_millis(event.time),
                        },
                    );
                }
            }
            RawSeatEventKind::PointerAxis { axis, .. } => {
                if let Some(frame) = super::super::seat::smithay_axis_frame(axis, event.time) {
                    pointer.axis(self, frame);
                }
            }
            _ => {}
        }
        pointer.frame(self);
    }
}
