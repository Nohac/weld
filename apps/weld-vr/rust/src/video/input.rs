//! Main-thread Godot input source. Engine events become owned values before
//! entering playback; this module knows nothing about transport or codecs.
mod cursor;

use crate::playback::Controller;
use godot::{
    classes::{
        Control, Input, InputEvent, InputEventKey, InputEventMouseButton, InputEventMouseMotion,
    },
    global::{Key, KeyLocation, MouseButton},
    prelude::*,
};
use std::collections::HashSet;
use weld_client::InputPosition;

pub(super) struct DesktopInput {
    view: Gd<Control>,
    // These are physical observations, not the accepted/suppressed holds in
    // playback::InputState. Even rejected presses may need reconciliation.
    keys: HashSet<(Key, KeyLocation)>,
    buttons: HashSet<MouseButton>,
}

impl DesktopInput {
    pub fn set_view(&mut self, view: Gd<Control>) {
        self.view = view;
    }
    pub fn new(view: Gd<Control>) -> Self {
        Self {
            view,
            keys: HashSet::new(),
            buttons: HashSet::new(),
        }
    }
    pub fn is_valid(&self) -> bool {
        self.view.is_instance_valid() && !self.view.is_queued_for_deletion()
    }
    pub fn handle(&mut self, event: Gd<InputEvent>, controller: &Controller) -> bool {
        let event = match event.try_cast::<InputEventKey>() {
            Ok(event) => {
                let key = (event.get_physical_keycode(), event.get_location());
                if event.is_pressed() {
                    self.keys.insert(key);
                } else {
                    self.keys.remove(&key);
                }
                return controller.key_input(
                    i64::from(key.0.ord()),
                    i64::from(key.1.ord()),
                    event.is_pressed(),
                    event.is_echo(),
                );
            }
            Err(event) => event,
        };
        let event = match event.try_cast::<InputEventMouseMotion>() {
            Ok(event) => {
                return self.pointer(controller, event.get_position(), MouseButton::NONE, false);
            }
            Err(event) => event,
        };
        if let Ok(event) = event.try_cast::<InputEventMouseButton>() {
            let button = event.get_button_index();
            if matches!(
                button,
                MouseButton::LEFT
                    | MouseButton::RIGHT
                    | MouseButton::MIDDLE
                    | MouseButton::XBUTTON1
                    | MouseButton::XBUTTON2
            ) {
                if event.is_pressed() {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
            }
            return self.pointer(controller, event.get_position(), button, event.is_pressed());
        }
        false
    }
    fn pointer(
        &self,
        controller: &Controller,
        position: Vector2,
        button: MouseButton,
        pressed: bool,
    ) -> bool {
        if !self.is_valid() {
            return false;
        }
        let rectangle = self.view.get_global_rect();
        controller.pointer_input(
            [
                f64::from(rectangle.position.x),
                f64::from(rectangle.position.y),
                f64::from(rectangle.size.x),
                f64::from(rectangle.size.y),
            ],
            InputPosition::new(f64::from(position.x), f64::from(position.y)),
            i64::from(button.ord()),
            pressed,
        )
    }
    pub fn pointer_left(&self, controller: &Controller) {
        if self.is_valid()
            && let Some(viewport) = self.view.get_viewport()
        {
            let position = viewport.get_mouse_position();
            controller.pointer_input(
                [0.0; 4],
                InputPosition::new(f64::from(position.x), f64::from(position.y)),
                0,
                false,
            );
        }
    }
    pub fn focus_lost(&self, controller: Option<&Controller>) {
        if let Some(controller) = controller {
            controller.reset_input();
        }
        cursor::reset();
    }
    pub fn reconcile_releases(&mut self, controller: &Controller) {
        let input = Input::singleton();
        self.keys.retain(|(key, location)| {
            if input.is_physical_key_pressed(*key) {
                return true;
            }
            controller.key_input(
                i64::from(key.ord()),
                i64::from(location.ord()),
                false,
                false,
            );
            false
        });
        // Collect before borrowing self again for geometry. There are at most
        // five tracked mouse buttons, and this only runs on focus regain.
        let released: Vec<_> = self
            .buttons
            .iter()
            .copied()
            .filter(|button| !input.is_mouse_button_pressed(*button))
            .collect();
        if self.is_valid()
            && let Some(viewport) = self.view.get_viewport()
        {
            for button in released {
                self.pointer(controller, viewport.get_mouse_position(), button, false);
                self.buttons.remove(&button);
            }
        }
    }
    pub fn stop(&mut self, controller: Option<&Controller>) {
        self.focus_lost(controller);
        self.keys.clear();
        self.buttons.clear();
    }
    pub fn update_cursor(&self, controller: &Controller) {
        if self.is_valid()
            && self
                .view
                .get_window()
                .is_some_and(|window| window.has_focus())
            && let Some(cursor) = controller.take_cursor()
        {
            cursor::apply(cursor);
        }
    }
}
