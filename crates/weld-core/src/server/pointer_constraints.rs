//! Native pointer constraints are enforced before application hit testing. Each
//! constraint belongs to the application's seat; remote seats cannot capture the
//! desktop pointer. Smithay owns protocol objects and their committed lifetime.

mod geometry;

use geometry::{confine, contains, inverse_transform};
use std::rc::Rc;

use smithay::{
    input::pointer::PointerHandle,
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::{Logical, Point, Rectangle},
    wayland::pointer_constraints::{
        ConstraintRemove, PointerConstraint, PointerConstraintsHandler, with_pointer_constraint,
    },
};
use weld_client::ClientPointerRoute;

use super::{ServerState, input_seat::InputSeat, surface_tree::effective_region};
use crate::input::{InputPosition, RawSeatEvent, RawSeatEventKind, SurfaceHit};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PointerConstraintMode {
    Locked,
    Confined,
}

pub(super) struct PointerConstraints {
    registered: Vec<(WlSurface, PointerHandle<ServerState>)>,
    warp: Option<SurfaceHit>,
    local_lock: bool,
    local_confine: bool,
}

impl Default for PointerConstraints {
    fn default() -> Self {
        Self {
            registered: Vec::new(),
            warp: None,
            local_lock: true,
            local_confine: true,
        }
    }
}

struct ActiveConstraint {
    mode: PointerConstraintMode,
    region: Vec<Rectangle<i32, Logical>>,
}

impl ServerState {
    pub(crate) fn configure_local_pointer_constraints(&mut self, lock: bool, confine: bool) {
        self.pointer_constraints.local_lock = lock;
        self.pointer_constraints.local_confine = confine;
        self.refresh_pointer_constraints();
    }
    fn input_for_pointer(&self, pointer: &PointerHandle<Self>) -> Option<Rc<InputSeat>> {
        self.input_seats()
            .find(|input| input.native.get_pointer().as_ref() == Some(pointer))
            .cloned()
    }

    fn constraint_region(
        &self,
        hit: SurfaceHit,
        surface: &WlSurface,
    ) -> &[Rectangle<i32, Logical>] {
        let tree = self
            .toplevels
            .get(hit.surface)
            .map(|window| &window.tree)
            .or_else(|| self.popups.get(hit.surface).map(|popup| &popup.tree))
            .or_else(|| self.layers.0.get(hit.surface).map(|layer| &layer.tree));
        let Some((current, region)) = tree.and_then(|tree| tree.input_region(hit.layer)) else {
            return &[];
        };
        if current != surface {
            return &[];
        }
        region
    }

    fn constraint_focus_allowed(&self, input: &InputSeat, hit: SurfaceHit) -> bool {
        let owner = self
            .popups
            .get(hit.surface)
            .and_then(|popup| popup.owner)
            .unwrap_or(hit.surface);
        input.focused_toplevel.get() == Some(owner)
            && (input.controller.get().is_some()
                || (!self.session_locked() && self.local_surface_is_visible(owner)))
    }

    pub(super) fn refresh_pointer_constraint(&self, input: &InputSeat) {
        let Some(pointer) = input.native.get_pointer() else {
            return;
        };
        let Some(surface) = pointer.current_focus() else {
            return;
        };
        let hit = input.pointer_target.get();
        with_pointer_constraint(&surface, &pointer, |constraint| {
            let Some(constraint) = constraint else {
                return;
            };
            let allowed = hit.is_some_and(|hit| {
                if !self.constraint_focus_allowed(input, hit) {
                    return false;
                }
                if input.controller.get().is_none()
                    && !match &*constraint {
                        PointerConstraint::Locked(_) => self.pointer_constraints.local_lock,
                        PointerConstraint::Confined(_) => self.pointer_constraints.local_confine,
                    }
                {
                    return false;
                }
                let region = self.constraint_region(hit, &surface);
                contains(region, hit.local_position)
                    && constraint.region().is_none_or(|region| {
                        region.contains(
                            Point::from((hit.local_position.x, hit.local_position.y))
                                .to_i32_floor(),
                        )
                    })
            });
            if allowed {
                constraint.activate();
            } else {
                constraint.deactivate();
            }
        });
    }

    pub(super) fn refresh_pointer_constraints(&mut self) {
        self.pointer_constraints
            .registered
            .retain(|(surface, pointer)| {
                surface.is_alive()
                    && with_pointer_constraint(surface, pointer, |constraint| constraint.is_some())
            });
        for (_, pointer) in &self.pointer_constraints.registered {
            if let Some(input) = self.input_for_pointer(pointer) {
                self.refresh_pointer_constraint(&input);
            }
        }
    }

    fn active_constraint(&self, input: &InputSeat) -> Option<ActiveConstraint> {
        let pointer = input.native.get_pointer()?;
        let surface = pointer.current_focus()?;
        let hit = input.pointer_target.get()?;
        with_pointer_constraint(&surface, &pointer, |constraint| {
            let constraint = constraint.filter(|constraint| constraint.is_active())?;
            let bounds = self.constraint_region(hit, &surface);
            let region = match &*constraint {
                PointerConstraint::Locked(_) => Vec::new(),
                PointerConstraint::Confined(_) => match constraint.region() {
                    Some(region) => bounds
                        .iter()
                        .copied()
                        .flat_map(|bounds| effective_region(region, bounds))
                        .collect(),
                    None => bounds.to_vec(),
                },
            };
            Some(ActiveConstraint {
                mode: match &*constraint {
                    PointerConstraint::Locked(_) => PointerConstraintMode::Locked,
                    PointerConstraint::Confined(_) => PointerConstraintMode::Confined,
                },
                region,
            })
        })
    }

    pub(crate) fn local_pointer_constraint(&self) -> Option<PointerConstraintMode> {
        let input = self.local_input.pointer_binding.borrow().clone()?;
        if input.controller.get().is_some() {
            return None;
        }
        self.active_constraint(&input)
            .map(|constraint| constraint.mode)
    }

    /// Correct desktop coordinates before policy sees motion. Device deltas are
    /// preserved verbatim for relative-pointer delivery, including at output edges.
    pub(crate) fn constrain_local_input(
        &mut self,
        mut event: RawSeatEvent,
        route: Option<ClientPointerRoute>,
    ) -> RawSeatEvent {
        if let RawSeatEventKind::PointerMotion { position, .. } = &mut event.event {
            if let Some(input) = self.local_input.pointer_binding.borrow().clone()
                && input.controller.get().is_none()
            {
                self.refresh_pointer_constraint(&input);
                if let Some(hit) = input.pointer_target.get()
                    && let Some(route) = route
                        .filter(|route| route.surface == hit.surface && route.layer == hit.layer)
                    && let Some(constraint) = self.active_constraint(&input)
                {
                    *position = match constraint.mode {
                        PointerConstraintMode::Locked => input.pointer_position.get(),
                        PointerConstraintMode::Confined => {
                            let local = confine(
                                hit.local_position,
                                route.transform.transform(*position),
                                &constraint.region,
                            );
                            inverse_transform(route.transform, local)
                                .unwrap_or(input.pointer_position.get())
                        }
                    };
                }
            }
            self.local_input.pointer_position.set(*position);
        }
        event
    }

    /// Enforce the same surface-local constraint for controller-addressed input.
    /// Returns whether absolute wl_pointer motion must be suppressed.
    pub(super) fn constrain_pointer_delivery(
        &self,
        input: &InputSeat,
        position: &mut InputPosition,
        target: &mut Option<SurfaceHit>,
    ) -> bool {
        self.refresh_pointer_constraint(input);
        let (Some(previous), Some(next)) = (input.pointer_target.get(), target.as_mut()) else {
            return false;
        };
        if previous.surface != next.surface || previous.layer != next.layer {
            return false;
        }
        let Some(constraint) = self.active_constraint(input) else {
            return false;
        };
        match constraint.mode {
            PointerConstraintMode::Locked => {
                *position = input.pointer_position.get();
                *next = previous;
                true
            }
            PointerConstraintMode::Confined => {
                next.local_position = confine(
                    previous.local_position,
                    next.local_position,
                    &constraint.region,
                );
                false
            }
        }
    }

    pub(crate) fn take_pointer_warp(
        &mut self,
        route: Option<ClientPointerRoute>,
    ) -> Option<RawSeatEvent> {
        let hint = self.pointer_constraints.warp.take()?;
        let input = self.local_input.pointer_binding.borrow().clone()?;
        let hit = input.pointer_target.get()?;
        if input.controller.get().is_some()
            || hit.surface != hint.surface
            || hit.layer != hint.layer
            || !self.constraint_focus_allowed(&input, hit)
            || self.active_constraint(&input).is_some()
        {
            return None;
        }
        let surface = input.native.get_pointer()?.current_focus()?;
        if !contains(self.constraint_region(hit, &surface), hint.local_position) {
            return None;
        }
        let route =
            route.filter(|route| route.surface == hint.surface && route.layer == hint.layer)?;
        let position = inverse_transform(route.transform, hint.local_position)?;
        Some(RawSeatEvent::new(
            RawSeatEventKind::PointerMotion {
                position,
                relative: None,
            },
            self.event_time(),
        ))
    }
}

impl PointerConstraintsHandler for ServerState {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        if !self
            .pointer_constraints
            .registered
            .iter()
            .any(|(current, handle)| current == surface && handle == pointer)
        {
            self.pointer_constraints
                .registered
                .push((surface.clone(), pointer.clone()));
        }
        if let Some(input) = self.input_for_pointer(pointer) {
            self.refresh_pointer_constraint(&input);
        }
    }

    fn remove_constraint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        reason: ConstraintRemove,
    ) {
        // PointerLeave is called while Smithay holds its pointer mutex. Only
        // client destruction may schedule a hint warp; policy focus loss must
        // not move the cursor back into a game that no longer owns input.
        let ConstraintRemove::Destroyed(constraint) = reason else {
            return;
        };
        self.pointer_constraints
            .registered
            .retain(|(current, handle)| current != surface || handle != pointer);
        let PointerConstraint::Locked(locked) = &constraint else {
            return;
        };
        if !constraint.is_active() {
            return;
        }
        let Some(hint) = locked.cursor_position_hint() else {
            return;
        };
        let Some(input) = self.input_for_pointer(pointer) else {
            return;
        };
        let local = self.local_input.pointer_binding.borrow().clone();
        if input.controller.get().is_some()
            || !local.is_some_and(|local| Rc::ptr_eq(&local, &input))
        {
            return;
        }
        let Some(mut hit) = input.pointer_target.get() else {
            return;
        };
        let position = InputPosition::new(hint.x, hint.y);
        if self.constraint_focus_allowed(&input, hit)
            && contains(self.constraint_region(hit, surface), position)
        {
            hit.local_position = position;
            self.pointer_constraints.warp = Some(hit);
        }
    }
}
