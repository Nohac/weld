//! Configuration-neutral global shortcut matching and press/release ownership.

use std::collections::HashSet;

use bevy::{
    app::{App, Plugin},
    ecs::{
        message::{Message, Messages},
        resource::Resource,
        world::World,
    },
    input::keyboard::KeyCode,
};

use crate::{ButtonState, LinuxKeycode, RawSeatEvent, RawSeatEventKind, SeatModifiers};
use bevy_winit::converters::convert_physical_key_code;
use winit::{keyboard::PhysicalKey, platform::scancode::PhysicalKeyExtScancode};

/// Physical keys whose release belongs to a shell shortcut.
#[derive(Resource, Default)]
pub struct ConsumedShortcutKeys(pub HashSet<LinuxKeycode>);

/// Modifier requirements for a shell-owned keyboard shortcut.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct GlobalShortcutModifiers {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
}

impl GlobalShortcutModifiers {
    /// Require the compositor Super modifier.
    pub const fn super_key() -> Self {
        Self {
            super_key: true,
            control: false,
            alt: false,
            shift: false,
        }
    }

    /// Require the compositor Super and Shift modifiers.
    pub const fn super_shift() -> Self {
        Self {
            shift: true,
            ..Self::super_key()
        }
    }

    fn matches(self, pressed: SeatModifiers) -> bool {
        (!self.control || pressed.control)
            && (!self.alt || pressed.alt)
            && (!self.shift || pressed.shift)
            && (!self.super_key || pressed.super_key)
    }
}

/// A physical keyboard chord consumed before client delivery.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GlobalShortcut {
    pub trigger: KeyCode,
    pub modifiers: GlobalShortcutModifiers,
}

impl GlobalShortcut {
    pub const fn new(trigger: KeyCode, modifiers: GlobalShortcutModifiers) -> Self {
        Self { trigger, modifiers }
    }
}

/// Opaque identity returned when an application shortcut is registered.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GlobalShortcutId(u64);

/// One registered application shortcut observed at raw-input pace.
#[derive(Clone, Copy, Debug, Message, PartialEq)]
pub struct GlobalShortcutPressed {
    shortcut: GlobalShortcutId,
}

impl GlobalShortcutPressed {
    pub const fn new(shortcut: GlobalShortcutId) -> Self {
        Self { shortcut }
    }

    pub const fn shortcut(self) -> GlobalShortcutId {
        self.shortcut
    }
}

/// Registers shell-owned keyboard shortcuts on a Bevy application.
pub trait GlobalShortcutAppExt {
    fn register_global_shortcut(&mut self, shortcut: GlobalShortcut) -> GlobalShortcutId;
}

impl GlobalShortcutAppExt for App {
    fn register_global_shortcut(&mut self, shortcut: GlobalShortcut) -> GlobalShortcutId {
        register(self);
        self.world_mut()
            .resource_mut::<GlobalShortcutRegistry>()
            .register(shortcut, false)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RegisteredGlobalShortcut {
    id: GlobalShortcutId,
    chord: GlobalShortcut,
    exact_modifiers: bool,
}

/// Owner-scoped live bindings. Replacement removes only this set, assigns fresh
/// IDs, and preserves consumed key releases. Already queued old IDs are obsolete.
#[derive(Default)]
pub struct GlobalShortcutSet(Vec<GlobalShortcutId>);

impl GlobalShortcutSet {
    /// Replaces bindings atomically through the shared registry. Call with an
    /// empty iterator to unregister the owned set.
    pub fn replace(
        &mut self,
        state: &mut GlobalShortcutRegistry,
        shortcuts: impl IntoIterator<Item = GlobalShortcut>,
    ) -> Vec<GlobalShortcutId> {
        state
            .application_shortcuts
            .retain(|entry| !self.0.contains(&entry.id));
        self.0 = shortcuts
            .into_iter()
            .map(|chord| state.register(chord, true))
            .collect();
        self.0.clone()
    }
}

/// Shared shortcut registry. Configuration systems borrow it with `ResMut` to
/// replace their owner-scoped bindings while preserving held-key consumption.
#[derive(Resource, Default)]
pub struct GlobalShortcutRegistry {
    next_id: u64,
    application_shortcuts: Vec<RegisteredGlobalShortcut>,
    pressed: HashSet<LinuxKeycode>,
}

impl GlobalShortcutRegistry {
    fn register(&mut self, chord: GlobalShortcut, exact_modifiers: bool) -> GlobalShortcutId {
        let id = GlobalShortcutId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        self.application_shortcuts.push(RegisteredGlobalShortcut {
            id,
            chord,
            exact_modifiers,
        });
        id
    }
}

/// Installs shortcut matching without any default chords or actions.
pub struct GlobalShortcutPlugin;

impl Plugin for GlobalShortcutPlugin {
    fn build(&self, app: &mut App) {
        register(app);
    }
}

fn register(app: &mut App) {
    app.init_resource::<ConsumedShortcutKeys>();
    app.init_resource::<GlobalShortcutRegistry>();
    if !app
        .world()
        .contains_resource::<Messages<GlobalShortcutPressed>>()
    {
        app.add_message::<GlobalShortcutPressed>();
    }
}

pub fn filter_global_shortcut_event(world: &mut World, event: &RawSeatEvent) -> bool {
    let RawSeatEventKind::Keyboard { keycode, state, .. } = &event.event else {
        if matches!(event.event, RawSeatEventKind::HostFocusLost)
            && let Some(mut shortcuts) = world.get_resource_mut::<GlobalShortcutRegistry>()
        {
            shortcuts.pressed.clear();
            if let Some(mut consumed) = world.get_resource_mut::<ConsumedShortcutKeys>() {
                consumed.0.clear();
            }
        }
        return false;
    };

    let Some(state) = state.transition() else {
        return world
            .get_resource::<ConsumedShortcutKeys>()
            .is_some_and(|consumed| consumed.0.contains(keycode));
    };
    let application_shortcut = {
        let Some(mut shortcuts) = world.get_resource_mut::<GlobalShortcutRegistry>() else {
            return false;
        };
        let newly_pressed = match state {
            ButtonState::Pressed => shortcuts.pressed.insert(*keycode),
            ButtonState::Released => {
                shortcuts.pressed.remove(keycode);
                false
            }
        };
        if !newly_pressed {
            None
        } else {
            let modifiers = event
                .modifiers
                .unwrap_or_else(|| SeatModifiers::from_pressed_keys(&shortcuts.pressed));
            let trigger = convert_physical_key_code(PhysicalKey::from_scancode(keycode.0));
            shortcuts
                .application_shortcuts
                .iter()
                .find(|shortcut| {
                    shortcut.chord.trigger == trigger
                        && shortcut.chord.modifiers.matches(modifiers)
                        && (!shortcut.exact_modifiers
                            || shortcut.chord.modifiers
                                == GlobalShortcutModifiers {
                                    control: modifiers.control,
                                    alt: modifiers.alt,
                                    shift: modifiers.shift,
                                    super_key: modifiers.super_key,
                                })
                })
                .map(|shortcut| shortcut.id)
        }
    };

    let consumed = world
        .get_resource::<ConsumedShortcutKeys>()
        .is_some_and(|consumed| consumed.0.contains(keycode));
    if application_shortcut.is_some() {
        if let Some(mut consumed) = world.get_resource_mut::<ConsumedShortcutKeys>() {
            consumed.0.insert(*keycode);
        }
        if let Some(shortcut) = application_shortcut
            && let Some(mut actions) = world.get_resource_mut::<Messages<GlobalShortcutPressed>>()
        {
            actions.write(GlobalShortcutPressed { shortcut });
        }
        true
    } else {
        if consumed
            && state == ButtonState::Released
            && let Some(mut consumed) = world.get_resource_mut::<ConsumedShortcutKeys>()
        {
            consumed.0.remove(keycode);
        }
        consumed
    }
}
