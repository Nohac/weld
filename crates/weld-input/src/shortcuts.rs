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
use winit::keyboard::{Key, NamedKey};
use winit::{keyboard::PhysicalKey, platform::scancode::PhysicalKeyExtScancode};

/// Physical binding identity for a Linux input key, including unnamed extended keys.
pub fn physical_keycode(keycode: LinuxKeycode) -> KeyCode {
    convert_physical_key_code(PhysicalKey::from_scancode(keycode.0))
}

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

/// A keyboard or modifier-only chord consumed before client delivery.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GlobalShortcut {
    /// A physical key, or `None` for a held modifier-only chord.
    pub trigger: Option<KeyCode>,
    pub modifiers: GlobalShortcutModifiers,
}

impl GlobalShortcut {
    pub const fn new(trigger: KeyCode, modifiers: GlobalShortcutModifiers) -> Self {
        Self {
            trigger: Some(trigger),
            modifiers,
        }
    }

    /// A modifier-only chord whose active state can be observed until release.
    pub const fn held(modifiers: GlobalShortcutModifiers) -> Self {
        Self {
            trigger: None,
            modifiers,
        }
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
        state.held.retain(|id| !self.ids.contains(id));
        state.enabled_holds.retain(|id| !self.ids.contains(id));
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
    held: HashSet<GlobalShortcutId>,
    enabled_holds: HashSet<GlobalShortcutId>,
    suppressed: SeatModifiers,
    modifiers: SeatModifiers,
}

impl GlobalShortcutRegistry {
    /// Whether the owner currently permits this modifier-only chord to activate.
    pub fn hold_enabled(&self, id: GlobalShortcutId) -> bool {
        self.enabled_holds.contains(&id)
    }
    fn update_holds(&mut self, modifiers: SeatModifiers) {
        self.modifiers = modifiers;
        self.held.clear();
        for shortcut in &self.application_shortcuts {
            if shortcut.chord.trigger.is_none()
                && self.enabled_holds.contains(&shortcut.id)
                && shortcut
                    .mode
                    .as_ref()
                    .is_none_or(|(group, mode)| self.active_modes.get(group) == Some(mode))
                && shortcut.chord.modifiers.matches(modifiers)
            {
                self.held.insert(shortcut.id);
                self.suppressed.control |= shortcut.chord.modifiers.control;
                self.suppressed.alt |= shortcut.chord.modifiers.alt;
                self.suppressed.shift |= shortcut.chord.modifiers.shift;
                self.suppressed.super_key |= shortcut.chord.modifiers.super_key;
            }
        }
        self.suppressed.control &= modifiers.control;
        self.suppressed.alt &= modifiers.alt;
        self.suppressed.shift &= modifiers.shift;
        self.suppressed.super_key &= modifiers.super_key;
    }

    /// Gate a modifier-only chord according to the current application context.
    pub fn set_hold_enabled(&mut self, id: GlobalShortcutId, enabled: bool) {
        if enabled {
            self.enabled_holds.insert(id);
            self.update_holds(self.modifiers);
        } else {
            self.enabled_holds.remove(&id);
            self.held.remove(&id);
        }
    }

    /// Whether an enabled modifier-only chord is physically held in the active mode.
    pub fn is_held(&self, id: GlobalShortcutId) -> bool {
        self.held.contains(&id)
    }

    /// Shortcut modifiers withheld from local applications until physically released.
    pub fn suppressed_modifiers(&self) -> SeatModifiers {
        self.suppressed
    }

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
            shortcuts.held.clear();
            shortcuts.suppressed = SeatModifiers::default();
            shortcuts.modifiers = SeatModifiers::default();
            if let Some(mut consumed) = world.get_resource_mut::<ConsumedShortcutKeys>() {
                consumed.0.clear();
            }
        } else if let Some(modifiers) = event.modifiers
            && world
                .get_resource::<GlobalShortcutRegistry>()
                .is_some_and(|shortcuts| shortcuts.modifiers != modifiers)
            && let Some(mut shortcuts) = world.get_resource_mut::<GlobalShortcutRegistry>()
        {
            shortcuts.update_holds(modifiers);
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
        let modifiers = event
            .modifiers
            .unwrap_or_else(|| SeatModifiers::from_pressed_keys(&shortcuts.pressed));
        shortcuts.update_holds(modifiers);
        if !newly_pressed {
            None
        } else {
            let modifiers = event
                .modifiers
                .unwrap_or_else(|| SeatModifiers::from_pressed_keys(&shortcuts.pressed));
            let trigger = physical_keycode(*keycode);
            let matched = shortcuts
                .application_shortcuts
                .iter()
                .find(|shortcut| {
                    shortcut.chord.trigger == Some(trigger)
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
    let modifier_consumed = state == ButtonState::Pressed
        && world
            .get_resource::<GlobalShortcutRegistry>()
            .is_some_and(|registry| match &event.event {
                RawSeatEventKind::Keyboard {
                    logical_key: Some(Key::Named(key)),
                    ..
                } => match key {
                    NamedKey::Control => registry.suppressed.control,
                    NamedKey::Super => registry.suppressed.super_key,
                    NamedKey::Alt => registry.suppressed.alt,
                    NamedKey::Shift => registry.suppressed.shift,
                    _ => false,
                },
                _ => false,
            });
    if application_shortcut.is_some() || modifier_consumed {
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
