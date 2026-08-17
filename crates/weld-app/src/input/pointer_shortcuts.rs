//! Declarative shell-owned pointer shortcuts.

use std::collections::HashSet;

use bevy::{
    app::App,
    ecs::{
        entity::Entity,
        message::{Message, Messages},
        resource::Resource,
        world::World,
    },
    input::mouse::MouseButton,
    math::Vec2,
};

use super::raw::{
    ButtonState, InputPosition, LinuxButtonCode, LinuxKeycode, RawSeatEvent, RawSeatEventKind,
};

/// Modifier requirements for a shell-owned pointer shortcut.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct PointerShortcutModifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
}

impl PointerShortcutModifiers {
    /// Require the compositor Super modifier.
    pub const fn super_key() -> Self {
        Self {
            super_key: true,
            control: false,
            alt: false,
            shift: false,
        }
    }
}

/// A pointer chord consumed by the shell before client delivery.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PointerShortcut {
    pub button: MouseButton,
    pub modifiers: PointerShortcutModifiers,
}

impl PointerShortcut {
    pub const fn new(button: MouseButton, modifiers: PointerShortcutModifiers) -> Self {
        Self { button, modifiers }
    }
}

/// Opaque identity returned when a pointer shortcut is registered.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PointerShortcutId(u64);

/// A raw-ingress shortcut decision retained for the next application frame.
#[derive(Clone, Copy, Debug, Message, PartialEq)]
pub struct PointerShortcutPressed {
    shortcut: PointerShortcutId,
    target: Option<Entity>,
    position: Option<Vec2>,
}

impl PointerShortcutPressed {
    pub const fn new(
        shortcut: PointerShortcutId,
        target: Option<Entity>,
        position: Option<Vec2>,
    ) -> Self {
        Self {
            shortcut,
            target,
            position,
        }
    }

    pub const fn shortcut(self) -> PointerShortcutId {
        self.shortcut
    }

    pub const fn target(self) -> Option<Entity> {
        self.target
    }

    /// Compositor-logical pointer position at the matching press.
    pub const fn position(self) -> Option<Vec2> {
        self.position
    }
}

/// Registers shell-owned pointer shortcuts on a Bevy application.
pub trait PointerShortcutAppExt {
    fn register_pointer_shortcut(&mut self, shortcut: PointerShortcut) -> PointerShortcutId;
}

impl PointerShortcutAppExt for App {
    fn register_pointer_shortcut(&mut self, shortcut: PointerShortcut) -> PointerShortcutId {
        register(self);
        let mut shortcuts = self.world_mut().resource_mut::<PointerShortcutRegistry>();
        shortcuts.register(shortcut)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RegisteredPointerShortcut {
    id: PointerShortcutId,
    chord: PointerShortcut,
}

#[derive(Resource, Default)]
struct PointerShortcutRegistry {
    next_id: u64,
    shortcuts: Vec<RegisteredPointerShortcut>,
    pressed_keys: HashSet<LinuxKeycode>,
    captured_buttons: HashSet<LinuxButtonCode>,
}

impl PointerShortcutRegistry {
    fn register(&mut self, chord: PointerShortcut) -> PointerShortcutId {
        let id = PointerShortcutId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        self.shortcuts.push(RegisteredPointerShortcut { id, chord });
        id
    }
}

#[derive(Resource, Default)]
pub(super) struct PublishedPointerTarget(pub(super) Option<Entity>);

pub(super) fn register(app: &mut App) {
    app.init_resource::<PointerShortcutRegistry>()
        .init_resource::<PublishedPointerTarget>();
    if !app
        .world()
        .contains_resource::<Messages<PointerShortcutPressed>>()
    {
        app.add_message::<PointerShortcutPressed>();
    }
}

pub(crate) fn filter_pointer_shortcut_event(world: &mut World, event: &RawSeatEvent) -> bool {
    match event.event {
        RawSeatEventKind::Keyboard { keycode, state, .. } => {
            if let Some(mut registry) = world.get_resource_mut::<PointerShortcutRegistry>() {
                match state {
                    ButtonState::Pressed => {
                        registry.pressed_keys.insert(keycode);
                    }
                    ButtonState::Released => {
                        registry.pressed_keys.remove(&keycode);
                    }
                }
            }
            false
        }
        RawSeatEventKind::PointerButton {
            position,
            button,
            state,
        } => filter_pointer_button(world, position, button, state),
        RawSeatEventKind::HostFocusLost => {
            if let Some(mut registry) = world.get_resource_mut::<PointerShortcutRegistry>() {
                registry.pressed_keys.clear();
                registry.captured_buttons.clear();
            }
            false
        }
        RawSeatEventKind::PointerMotion { .. } => world
            .get_resource::<PointerShortcutRegistry>()
            .is_some_and(|registry| !registry.captured_buttons.is_empty()),
        RawSeatEventKind::PointerLeft { .. }
        | RawSeatEventKind::PointerAxis { .. }
        | RawSeatEventKind::PointerGesture { .. } => false,
    }
}

fn filter_pointer_button(
    world: &mut World,
    position: Option<InputPosition>,
    button: LinuxButtonCode,
    state: ButtonState,
) -> bool {
    if state == ButtonState::Released {
        return world
            .get_resource_mut::<PointerShortcutRegistry>()
            .is_some_and(|mut registry| registry.captured_buttons.remove(&button));
    }

    let matches = {
        let Some(registry) = world.get_resource::<PointerShortcutRegistry>() else {
            return false;
        };
        registry
            .shortcuts
            .iter()
            .filter(|&&shortcut| shortcut_matches(shortcut, button, &registry.pressed_keys))
            .map(|shortcut| shortcut.id)
            .collect::<Vec<_>>()
    };
    if matches.is_empty() {
        return false;
    }

    if let Some(mut registry) = world.get_resource_mut::<PointerShortcutRegistry>() {
        registry.captured_buttons.insert(button);
    }
    let target = world
        .get_resource::<PublishedPointerTarget>()
        .and_then(|target| target.0);
    let position = position.and_then(pointer_position);
    if let Some(mut messages) = world.get_resource_mut::<Messages<PointerShortcutPressed>>() {
        for shortcut in matches {
            messages.write(PointerShortcutPressed {
                shortcut,
                target,
                position,
            });
        }
    }
    true
}

fn pointer_position(position: InputPosition) -> Option<Vec2> {
    let position = Vec2::new(position.x as f32, position.y as f32);
    position.is_finite().then_some(position)
}

fn shortcut_matches(
    shortcut: RegisteredPointerShortcut,
    button: LinuxButtonCode,
    pressed_keys: &HashSet<LinuxKeycode>,
) -> bool {
    linux_button(shortcut.chord.button) == Some(button)
        && modifiers_pressed(shortcut.chord.modifiers, pressed_keys)
}

fn linux_button(button: MouseButton) -> Option<LinuxButtonCode> {
    match button {
        MouseButton::Left => Some(LinuxButtonCode(0x110)),
        MouseButton::Right => Some(LinuxButtonCode(0x111)),
        MouseButton::Middle => Some(LinuxButtonCode(0x112)),
        MouseButton::Back => Some(LinuxButtonCode(0x113)),
        MouseButton::Forward => Some(LinuxButtonCode(0x114)),
        MouseButton::Other(_) => None,
    }
}

fn modifiers_pressed(
    required: PointerShortcutModifiers,
    pressed_keys: &HashSet<LinuxKeycode>,
) -> bool {
    (!required.control || any_pressed(pressed_keys, &[29, 97]))
        && (!required.alt || any_pressed(pressed_keys, &[56, 100]))
        && (!required.shift || any_pressed(pressed_keys, &[42, 54]))
        && (!required.super_key || any_pressed(pressed_keys, &[125, 126]))
}

fn any_pressed(pressed_keys: &HashSet<LinuxKeycode>, keycodes: &[u32]) -> bool {
    keycodes
        .iter()
        .any(|keycode| pressed_keys.contains(&LinuxKeycode(*keycode)))
}

#[cfg(test)]
mod tests {
    use bevy::ecs::message::MessageCursor;

    use super::*;
    use crate::input::raw::InputPosition;

    fn keyboard(keycode: u32, state: ButtonState) -> RawSeatEvent {
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(keycode),
                logical_key: None,
                state,
            },
            1,
        )
    }

    fn primary(state: ButtonState) -> RawSeatEvent {
        RawSeatEvent::new(
            RawSeatEventKind::PointerButton {
                position: Some(InputPosition::new(10.0, 20.0)),
                button: LinuxButtonCode(0x110),
                state,
            },
            2,
        )
    }

    fn secondary(state: ButtonState) -> RawSeatEvent {
        RawSeatEvent::new(
            RawSeatEventKind::PointerButton {
                position: Some(InputPosition::new(30.0, 40.0)),
                button: LinuxButtonCode(0x111),
                state,
            },
            2,
        )
    }

    fn motion() -> RawSeatEvent {
        RawSeatEvent::new(
            RawSeatEventKind::PointerMotion {
                position: InputPosition::new(12.0, 22.0),
            },
            3,
        )
    }

    fn shortcut_app() -> (App, PointerShortcutId, Entity) {
        let mut app = App::new();
        let shortcut = app.register_pointer_shortcut(PointerShortcut::new(
            MouseButton::Left,
            PointerShortcutModifiers::super_key(),
        ));
        let target = app.world_mut().spawn_empty().id();
        app.world_mut().resource_mut::<PublishedPointerTarget>().0 = Some(target);
        (app, shortcut, target)
    }

    #[test]
    fn raw_decision_consumes_the_matching_press_and_paired_release() {
        let (mut app, shortcut, target) = shortcut_app();
        let mut presses = MessageCursor::<PointerShortcutPressed>::default();

        assert!(!filter_pointer_shortcut_event(
            app.world_mut(),
            &keyboard(125, ButtonState::Pressed),
        ));
        assert!(filter_pointer_shortcut_event(
            app.world_mut(),
            &primary(ButtonState::Pressed),
        ));
        assert!(filter_pointer_shortcut_event(app.world_mut(), &motion()));
        assert!(!filter_pointer_shortcut_event(
            app.world_mut(),
            &keyboard(125, ButtonState::Released),
        ));
        assert!(filter_pointer_shortcut_event(
            app.world_mut(),
            &primary(ButtonState::Released),
        ));
        assert!(!filter_pointer_shortcut_event(app.world_mut(), &motion()));

        assert_eq!(
            presses
                .read(app.world().resource::<Messages<PointerShortcutPressed>>())
                .copied()
                .collect::<Vec<_>>(),
            [PointerShortcutPressed {
                shortcut,
                target: Some(target),
                position: Some(Vec2::new(10.0, 20.0)),
            }]
        );
    }

    #[test]
    fn event_order_and_focus_loss_prevent_stale_shortcut_capture() {
        let (mut app, _, _) = shortcut_app();

        assert!(!filter_pointer_shortcut_event(
            app.world_mut(),
            &primary(ButtonState::Pressed),
        ));
        assert!(!filter_pointer_shortcut_event(app.world_mut(), &motion()));
        assert!(!filter_pointer_shortcut_event(
            app.world_mut(),
            &keyboard(125, ButtonState::Pressed),
        ));
        assert!(!filter_pointer_shortcut_event(
            app.world_mut(),
            &primary(ButtonState::Released),
        ));

        assert!(filter_pointer_shortcut_event(
            app.world_mut(),
            &primary(ButtonState::Pressed),
        ));
        assert!(!filter_pointer_shortcut_event(
            app.world_mut(),
            &RawSeatEvent::new(RawSeatEventKind::HostFocusLost, 3),
        ));
        assert!(!filter_pointer_shortcut_event(
            app.world_mut(),
            &primary(ButtonState::Released),
        ));
    }

    #[test]
    fn registered_buttons_keep_distinct_shortcut_identity() {
        let (mut app, move_shortcut, target) = shortcut_app();
        let resize_shortcut = app.register_pointer_shortcut(PointerShortcut::new(
            MouseButton::Right,
            PointerShortcutModifiers::super_key(),
        ));
        let mut presses = MessageCursor::<PointerShortcutPressed>::default();

        filter_pointer_shortcut_event(app.world_mut(), &keyboard(125, ButtonState::Pressed));
        assert!(filter_pointer_shortcut_event(
            app.world_mut(),
            &secondary(ButtonState::Pressed),
        ));

        assert_eq!(
            presses
                .read(app.world().resource::<Messages<PointerShortcutPressed>>())
                .copied()
                .collect::<Vec<_>>(),
            [PointerShortcutPressed::new(
                resize_shortcut,
                Some(target),
                Some(Vec2::new(30.0, 40.0)),
            )]
        );
        assert_ne!(move_shortcut, resize_shortcut);
    }
}
