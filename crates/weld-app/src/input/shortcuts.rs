//! Leafwing-backed global shortcuts and host commands.

use std::collections::{HashSet, VecDeque};

use bevy::{
    app::{App, Plugin},
    ecs::{
        message::{Message, Messages},
        resource::Resource,
        world::World,
    },
};

use super::{
    raw::{ButtonState, LinuxKeycode, RawSeatEvent, RawSeatEventKind},
    state::ConsumedShortcutKeys,
};
use crate::{ActiveBackend, WeldAppExt};
use weld_core::runtime::{HostCommand, OutputScaleAdjustment};

/// Application-owned action produced by a consumed global shortcut.
#[derive(Clone, Copy, Debug, Eq, Message, PartialEq)]
pub enum GlobalShortcutAction {
    ToggleOutputTopology,
}

#[derive(Clone, Copy)]
enum GlobalShortcutCommand {
    Launch(&'static str),
    AdjustOutputScale(OutputScaleAdjustment),
    MatchPhysicalScale,
    Application(GlobalShortcutAction),
    Exit,
}

#[derive(Clone, Copy)]
struct GlobalShortcutDefinition {
    trigger: LinuxKeycode,
    shift: bool,
    drm_only: bool,
    command: GlobalShortcutCommand,
}

impl GlobalShortcutDefinition {
    fn host_command(self) -> Option<HostCommand> {
        match self.command {
            GlobalShortcutCommand::Launch(program) => Some(HostCommand::Launch {
                program: program.into(),
                arguments: Vec::new(),
            }),
            GlobalShortcutCommand::AdjustOutputScale(adjustment) => {
                Some(HostCommand::AdjustOutputScale(adjustment))
            }
            GlobalShortcutCommand::MatchPhysicalScale => {
                Some(HostCommand::MatchOutputPhysicalScale)
            }
            GlobalShortcutCommand::Application(_) => None,
            GlobalShortcutCommand::Exit => Some(HostCommand::Exit),
        }
    }

    fn application_action(self) -> Option<GlobalShortcutAction> {
        match self.command {
            GlobalShortcutCommand::Application(action) => Some(action),
            GlobalShortcutCommand::Launch(_)
            | GlobalShortcutCommand::AdjustOutputScale(_)
            | GlobalShortcutCommand::MatchPhysicalScale
            | GlobalShortcutCommand::Exit => None,
        }
    }
}

const GLOBAL_SHORTCUTS: [GlobalShortcutDefinition; 8] = [
    GlobalShortcutDefinition {
        trigger: LinuxKeycode(28),
        shift: false,
        drm_only: false,
        command: GlobalShortcutCommand::Launch("foot"),
    },
    GlobalShortcutDefinition {
        trigger: LinuxKeycode(33),
        shift: false,
        drm_only: false,
        command: GlobalShortcutCommand::Launch("firefox"),
    },
    GlobalShortcutDefinition {
        trigger: LinuxKeycode(48),
        shift: false,
        drm_only: false,
        command: GlobalShortcutCommand::Launch("blender"),
    },
    GlobalShortcutDefinition {
        trigger: LinuxKeycode(13),
        shift: false,
        drm_only: true,
        command: GlobalShortcutCommand::AdjustOutputScale(OutputScaleAdjustment::Increase),
    },
    GlobalShortcutDefinition {
        trigger: LinuxKeycode(12),
        shift: false,
        drm_only: true,
        command: GlobalShortcutCommand::AdjustOutputScale(OutputScaleAdjustment::Decrease),
    },
    GlobalShortcutDefinition {
        trigger: LinuxKeycode(32),
        shift: true,
        drm_only: true,
        command: GlobalShortcutCommand::MatchPhysicalScale,
    },
    GlobalShortcutDefinition {
        trigger: LinuxKeycode(24),
        shift: true,
        drm_only: false,
        command: GlobalShortcutCommand::Application(GlobalShortcutAction::ToggleOutputTopology),
    },
    GlobalShortcutDefinition {
        trigger: LinuxKeycode(1),
        shift: true,
        drm_only: false,
        command: GlobalShortcutCommand::Exit,
    },
];

#[derive(Resource, Default)]
struct GlobalHostCommands(VecDeque<HostCommand>);

#[derive(Resource)]
struct RawGlobalShortcutState {
    shortcuts: Vec<GlobalShortcutDefinition>,
    pressed: HashSet<LinuxKeycode>,
}

pub struct GlobalShortcutPlugin;

impl Plugin for GlobalShortcutPlugin {
    fn build(&self, app: &mut App) {
        let backend = app.backend();
        let shortcuts = GLOBAL_SHORTCUTS
            .into_iter()
            .filter(|shortcut| !shortcut.drm_only || backend == Some(ActiveBackend::Drm))
            .collect::<Vec<_>>();
        app.add_message::<GlobalShortcutAction>()
            .init_resource::<GlobalHostCommands>()
            .insert_resource(RawGlobalShortcutState {
                shortcuts,
                pressed: HashSet::new(),
            });
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

    let command = {
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
            None
        } else {
            let super_pressed = modifier_pressed(&shortcuts.pressed, &[125, 126]);
            let shift_pressed = modifier_pressed(&shortcuts.pressed, &[42, 54]);
            shortcuts
                .shortcuts
                .iter()
                .find(|shortcut| {
                    shortcut.trigger == *keycode
                        && super_pressed
                        && (!shortcut.shift || shift_pressed)
                })
                .copied()
        }
    };

    let consumed = world
        .get_resource::<ConsumedShortcutKeys>()
        .is_some_and(|consumed| consumed.0.contains(keycode));
    if let Some(shortcut) = command {
        if let Some(mut consumed) = world.get_resource_mut::<ConsumedShortcutKeys>() {
            consumed.0.insert(*keycode);
        }
        if let Some(command) = shortcut.host_command()
            && let Some(mut commands) = world.get_resource_mut::<GlobalHostCommands>()
        {
            commands.0.push_back(command);
        }
        if let Some(action) = shortcut.application_action()
            && let Some(mut actions) = world.get_resource_mut::<Messages<GlobalShortcutAction>>()
        {
            actions.write(action);
        }
        true
    } else {
        if consumed
            && *state == ButtonState::Released
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

pub(crate) fn take_host_commands(world: &mut World) -> Vec<HostCommand> {
    world
        .get_resource_mut::<GlobalHostCommands>()
        .map(|mut commands| commands.0.drain(..).collect())
        .unwrap_or_default()
}
