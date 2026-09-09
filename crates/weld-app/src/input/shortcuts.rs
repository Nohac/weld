//! Shell-owned global shortcuts and host commands.

use std::collections::{HashSet, VecDeque};

use bevy::{
    app::{App, Plugin},
    ecs::{
        message::{Message, Messages},
        resource::Resource,
        world::World,
    },
    input::keyboard::KeyCode,
};

use super::{
    projection::bevy_keycode,
    raw::{ButtonState, LinuxKeycode, RawSeatEvent, RawSeatEventKind},
    state::ConsumedShortcutKeys,
};
use crate::{ActiveBackend, WeldAppExt};
use weld_core::runtime::{HostCommand, OutputScaleAdjustment};

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

    fn matches(self, pressed: &HashSet<LinuxKeycode>) -> bool {
        (!self.control || modifier_pressed(pressed, &[29, 97]))
            && (!self.alt || modifier_pressed(pressed, &[56, 100]))
            && (!self.shift || modifier_pressed(pressed, &[42, 54]))
            && (!self.super_key || modifier_pressed(pressed, &[125, 126]))
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
            .resource_mut::<RawGlobalShortcutState>()
            .register(shortcut)
    }
}

#[derive(Clone, Copy)]
enum GlobalShortcutCommand {
    Launch(&'static str),
    AdjustOutputScale(OutputScaleAdjustment),
    MatchPhysicalScale,
    Exit,
}

#[derive(Clone, Copy)]
struct HostShortcutDefinition {
    trigger: LinuxKeycode,
    shift: bool,
    drm_only: bool,
    command: GlobalShortcutCommand,
}

impl HostShortcutDefinition {
    fn host_command(self) -> HostCommand {
        match self.command {
            GlobalShortcutCommand::Launch(program) => HostCommand::Launch {
                program: program.into(),
                arguments: Vec::new(),
            },
            GlobalShortcutCommand::AdjustOutputScale(adjustment) => {
                HostCommand::AdjustOutputScale(adjustment)
            }
            GlobalShortcutCommand::MatchPhysicalScale => HostCommand::MatchOutputPhysicalScale,
            GlobalShortcutCommand::Exit => HostCommand::Exit,
        }
    }
}

const HOST_SHORTCUTS: [HostShortcutDefinition; 7] = [
    HostShortcutDefinition {
        trigger: LinuxKeycode(28),
        shift: false,
        drm_only: false,
        command: GlobalShortcutCommand::Launch("foot"),
    },
    HostShortcutDefinition {
        trigger: LinuxKeycode(33),
        shift: false,
        drm_only: false,
        command: GlobalShortcutCommand::Launch("firefox"),
    },
    HostShortcutDefinition {
        trigger: LinuxKeycode(48),
        shift: false,
        drm_only: false,
        command: GlobalShortcutCommand::Launch("blender"),
    },
    HostShortcutDefinition {
        trigger: LinuxKeycode(13),
        shift: false,
        drm_only: true,
        command: GlobalShortcutCommand::AdjustOutputScale(OutputScaleAdjustment::Increase),
    },
    HostShortcutDefinition {
        trigger: LinuxKeycode(12),
        shift: false,
        drm_only: true,
        command: GlobalShortcutCommand::AdjustOutputScale(OutputScaleAdjustment::Decrease),
    },
    HostShortcutDefinition {
        trigger: LinuxKeycode(32),
        shift: true,
        drm_only: true,
        command: GlobalShortcutCommand::MatchPhysicalScale,
    },
    HostShortcutDefinition {
        trigger: LinuxKeycode(1),
        shift: true,
        drm_only: false,
        command: GlobalShortcutCommand::Exit,
    },
];

#[derive(Resource, Default)]
struct GlobalHostCommands(VecDeque<HostCommand>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RegisteredGlobalShortcut {
    id: GlobalShortcutId,
    chord: GlobalShortcut,
}

#[derive(Resource, Default)]
struct RawGlobalShortcutState {
    next_id: u64,
    host_shortcuts: Vec<HostShortcutDefinition>,
    application_shortcuts: Vec<RegisteredGlobalShortcut>,
    pressed: HashSet<LinuxKeycode>,
    host_shortcuts_registered: bool,
}

impl RawGlobalShortcutState {
    fn register(&mut self, chord: GlobalShortcut) -> GlobalShortcutId {
        let id = GlobalShortcutId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        self.application_shortcuts
            .push(RegisteredGlobalShortcut { id, chord });
        id
    }

    fn register_host_shortcuts(&mut self, backend: Option<ActiveBackend>) {
        if self.host_shortcuts_registered {
            return;
        }
        self.host_shortcuts.extend(
            HOST_SHORTCUTS
                .into_iter()
                .filter(|shortcut| !shortcut.drm_only || backend == Some(ActiveBackend::Drm)),
        );
        self.host_shortcuts_registered = true;
    }
}

pub struct GlobalShortcutPlugin;

impl Plugin for GlobalShortcutPlugin {
    fn build(&self, app: &mut App) {
        let backend = app.backend();
        register(app);
        app.world_mut()
            .resource_mut::<RawGlobalShortcutState>()
            .register_host_shortcuts(backend);
    }
}

fn register(app: &mut App) {
    app.init_resource::<GlobalHostCommands>()
        .init_resource::<RawGlobalShortcutState>();
    if !app
        .world()
        .contains_resource::<Messages<GlobalShortcutPressed>>()
    {
        app.add_message::<GlobalShortcutPressed>();
    }
}

pub(crate) fn filter_global_shortcut_event(world: &mut World, event: &RawSeatEvent) -> bool {
    let RawSeatEventKind::Keyboard { keycode, state, .. } = &event.event else {
        if matches!(event.event, RawSeatEventKind::HostFocusLost)
            && let Some(mut shortcuts) = world.get_resource_mut::<RawGlobalShortcutState>()
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
    let (host_command, application_shortcut) = {
        let Some(mut shortcuts) = world.get_resource_mut::<RawGlobalShortcutState>() else {
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
            (None, None)
        } else {
            let super_pressed = modifier_pressed(&shortcuts.pressed, &[125, 126]);
            let shift_pressed = modifier_pressed(&shortcuts.pressed, &[42, 54]);
            let host = shortcuts
                .host_shortcuts
                .iter()
                .find(|shortcut| {
                    shortcut.trigger == *keycode
                        && super_pressed
                        && (!shortcut.shift || shift_pressed)
                })
                .copied();
            let application = host.is_none().then(|| {
                let trigger = bevy_keycode(*keycode);
                shortcuts
                    .application_shortcuts
                    .iter()
                    .find(|shortcut| {
                        shortcut.chord.trigger == trigger
                            && shortcut.chord.modifiers.matches(&shortcuts.pressed)
                    })
                    .map(|shortcut| shortcut.id)
            });
            (
                host.map(HostShortcutDefinition::host_command),
                application.flatten(),
            )
        }
    };

    let consumed = world
        .get_resource::<ConsumedShortcutKeys>()
        .is_some_and(|consumed| consumed.0.contains(keycode));
    if host_command.is_some() || application_shortcut.is_some() {
        if let Some(mut consumed) = world.get_resource_mut::<ConsumedShortcutKeys>() {
            consumed.0.insert(*keycode);
        }
        if let Some(command) = host_command
            && let Some(mut commands) = world.get_resource_mut::<GlobalHostCommands>()
        {
            commands.0.push_back(command);
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

fn modifier_pressed(pressed: &HashSet<LinuxKeycode>, keycodes: &[u32]) -> bool {
    keycodes
        .iter()
        .any(|keycode| pressed.contains(&LinuxKeycode(*keycode)))
}

pub(super) fn take_shortcut_commands(world: &mut World) -> Vec<HostCommand> {
    world
        .get_resource_mut::<GlobalHostCommands>()
        .map(|mut commands| commands.0.drain(..).collect())
        .unwrap_or_default()
}
