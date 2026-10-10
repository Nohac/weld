//! Configuration-neutral global shortcut matching and press/release ownership.

use anyhow::{Result, ensure};
use std::collections::{HashMap, HashSet};

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

#[derive(Clone, Debug, Eq, PartialEq)]
struct RegisteredGlobalShortcut {
    id: GlobalShortcutId,
    chord: GlobalShortcut,
    exact_modifiers: bool,
    mode: Option<(GlobalShortcutId, String)>,
    switch_to: Option<String>,
}

/// One named, mutually exclusive set of chords. A transition takes effect at
/// raw-input pace, before the next event in the same host dispatch batch.
pub struct GlobalShortcutMode {
    pub name: String,
    pub bindings: Vec<(GlobalShortcut, Option<String>)>,
}

/// Owner-scoped live bindings. Replacement removes only this set, assigns fresh
/// IDs, and preserves consumed key releases. Already queued old IDs are obsolete.
#[derive(Default)]
pub struct GlobalShortcutSet {
    ids: Vec<GlobalShortcutId>,
    mode_group: Option<GlobalShortcutId>,
}

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
            .retain(|entry| !self.ids.contains(&entry.id));
        if let Some(group) = self.mode_group.take() {
            state.active_modes.remove(&group);
        }
        self.ids = shortcuts
            .into_iter()
            .map(|chord| state.register(chord, true))
            .collect();
        self.ids.clone()
    }

    /// Replace all modes atomically, retaining the active mode if still defined.
    /// Returns IDs in the same mode/binding order as the input.
    pub fn replace_modes(
        &mut self,
        state: &mut GlobalShortcutRegistry,
        modes: Vec<GlobalShortcutMode>,
    ) -> Result<Vec<Vec<GlobalShortcutId>>> {
        let names: HashSet<_> = modes.iter().map(|mode| mode.name.as_str()).collect();
        ensure!(
            names.len() == modes.len() && names.contains("default"),
            "shortcut modes need unique names and a default mode"
        );
        for mode in &modes {
            let mut chords = HashSet::new();
            for (chord, target) in &mode.bindings {
                ensure!(chords.insert(*chord), "duplicate chord in shortcut mode");
                ensure!(
                    target
                        .as_deref()
                        .is_none_or(|target| names.contains(target)),
                    "shortcut transition targets an undefined mode"
                );
            }
        }
        let active = self
            .active_mode(state)
            .filter(|active| names.contains(active))
            .unwrap_or("default")
            .to_owned();
        self.replace(state, []);
        let group = GlobalShortcutId(state.next_id);
        state.next_id = state.next_id.saturating_add(1);
        state.active_modes.insert(group, active);
        self.mode_group = Some(group);
        let mut result = Vec::with_capacity(modes.len());
        for mode in modes {
            let mut ids = Vec::with_capacity(mode.bindings.len());
            for (chord, switch_to) in mode.bindings {
                let id = state.register(chord, true);
                if let Some(entry) = state.application_shortcuts.last_mut() {
                    entry.mode = Some((group, mode.name.clone()));
                    entry.switch_to = switch_to;
                }
                self.ids.push(id);
                ids.push(id);
            }
            result.push(ids);
        }
        Ok(result)
    }

    pub fn active_mode<'a>(&self, state: &'a GlobalShortcutRegistry) -> Option<&'a str> {
        state
            .active_modes
            .get(&self.mode_group?)
            .map(String::as_str)
    }
}

/// Shared shortcut registry. Configuration systems borrow it with `ResMut` to
/// replace their owner-scoped bindings while preserving held-key consumption.
#[derive(Resource, Default)]
pub struct GlobalShortcutRegistry {
    next_id: u64,
    application_shortcuts: Vec<RegisteredGlobalShortcut>,
    pressed: HashSet<LinuxKeycode>,
    active_modes: HashMap<GlobalShortcutId, String>,
}

impl GlobalShortcutRegistry {
    fn register(&mut self, chord: GlobalShortcut, exact_modifiers: bool) -> GlobalShortcutId {
        let id = GlobalShortcutId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        self.application_shortcuts.push(RegisteredGlobalShortcut {
            id,
            chord,
            exact_modifiers,
            mode: None,
            switch_to: None,
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
            let matched = shortcuts
                .application_shortcuts
                .iter()
                .find(|shortcut| {
                    shortcut.chord.trigger == trigger
                        && shortcut.mode.as_ref().is_none_or(|(group, mode)| {
                            shortcuts.active_modes.get(group) == Some(mode)
                        })
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
                .cloned();
            if let Some(shortcut) = &matched
                && let Some((group, _)) = &shortcut.mode
                && let Some(target) = &shortcut.switch_to
            {
                shortcuts.active_modes.insert(*group, target.clone());
            }
            matched.map(|shortcut| shortcut.id)
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
