//! Contact capture and coordinate transforms shared by input producers.

use crate::{
    ClientInputDispatchResult, ClientInputEvent, ClientInputTarget, ClientPointerRoute,
    ClientRuntime, ClientSurfaceId, InputEventKind, MAX_TOUCH_CONTACTS, TouchEvent, TouchId,
};

#[derive(Default)]
pub(super) struct TouchRouting {
    contacts: Vec<(TouchId, ClientPointerRoute)>,
    pending: Vec<ClientPointerRoute>,
    time: u32,
}

impl ClientRuntime {
    /// Route touch independently of pointer hover. Only down uses `route`;
    /// subsequent contact events keep their original target and transform.
    /// Frame and cancel fan out to the targets participating in the interaction.
    pub fn dispatch_touch(
        &mut self,
        route: Option<ClientPointerRoute>,
        event: TouchEvent,
        time: u32,
    ) -> ClientInputDispatchResult {
        if !event.is_valid() {
            return ClientInputDispatchResult::NoRoute;
        }
        self.touch.time = time;
        if matches!(event, TouchEvent::Frame | TouchEvent::Cancel) {
            let mut routes = std::mem::take(&mut self.touch.pending);
            if event == TouchEvent::Cancel {
                for (_, route) in self.touch.contacts.drain(..) {
                    if !routes.contains(&route) {
                        routes.push(route);
                    }
                }
            }
            let mut result = ClientInputDispatchResult::NoRoute;
            for route in routes.drain(..) {
                result = self.send_touch(route, event, time);
            }
            self.touch.pending = routes;
            return result;
        }
        let Some(id) = event.id() else {
            return ClientInputDispatchResult::NoRoute;
        };
        let index = self
            .touch
            .contacts
            .iter()
            .position(|(current, _)| *current == id);
        let route = match event {
            TouchEvent::Down { .. } => {
                if index.is_some() || self.touch.contacts.len() >= MAX_TOUCH_CONTACTS {
                    return ClientInputDispatchResult::NoRoute;
                }
                let Some(route) = self.resolved_pointer_route(route).ok().flatten() else {
                    return ClientInputDispatchResult::NoRoute;
                };
                if self.touch.pending.len() >= MAX_TOUCH_CONTACTS
                    && !self.touch.pending.contains(&route)
                {
                    return ClientInputDispatchResult::NoRoute;
                }
                self.touch.contacts.push((id, route));
                route
            }
            _ => {
                let Some(index) = index else {
                    return ClientInputDispatchResult::NoRoute;
                };
                let (_, route) = self.touch.contacts[index];
                if matches!(event, TouchEvent::Up { .. }) {
                    self.touch.contacts.remove(index);
                }
                route
            }
        };
        if !self.touch.pending.contains(&route) {
            self.touch.pending.push(route);
        }
        let event = match event {
            TouchEvent::Down { id, position } => TouchEvent::Down {
                id,
                position: route.transform.transform(position),
            },
            TouchEvent::Motion { id, position } => TouchEvent::Motion {
                id,
                position: route.transform.transform(position),
            },
            event => event,
        };
        if !event.is_valid() {
            self.cancel_surface_touch(route.surface);
            return ClientInputDispatchResult::NoRoute;
        }
        let result = self.send_touch(route, event, time);
        if result != ClientInputDispatchResult::Delivered {
            self.touch.contacts.retain(|(current, _)| *current != id);
            self.touch.pending.retain(|current| *current != route);
        }
        result
    }

    pub(super) fn cancel_surface_touch(&mut self, surface: ClientSurfaceId) {
        let mut routes = Vec::new();
        for route in self
            .touch
            .contacts
            .iter()
            .map(|(_, route)| route)
            .chain(&self.touch.pending)
        {
            if route.surface == surface && !routes.contains(route) {
                routes.push(*route);
            }
        }
        self.touch
            .contacts
            .retain(|(_, route)| route.surface != surface);
        self.touch.pending.retain(|route| route.surface != surface);
        for route in routes {
            self.send_touch(route, TouchEvent::Cancel, self.touch.time);
        }
    }

    fn send_touch(
        &mut self,
        route: ClientPointerRoute,
        event: TouchEvent,
        time: u32,
    ) -> ClientInputDispatchResult {
        self.dispatch_to(
            route.surface.source(),
            ClientInputEvent {
                target: ClientInputTarget::Touch {
                    surface: route.surface,
                    layer: route.layer,
                },
                host_position: None,
                event: InputEventKind::Touch { event },
                time,
            },
        )
    }
}
