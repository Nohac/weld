//! Leafwing-backed global shortcuts and host commands.

use std::collections::{HashSet, VecDeque};

use bevy::{
    app::{App, Plugin},
    ecs::{resource::Resource, world::World},
    input::keyboard::KeyCode,
    prelude::Reflect,
};
use leafwing_input_manager::{
    plugin::InputManagerPlugin,
    prelude::{Actionlike, ButtonlikeChord, InputMap, ModifierKey},
};

use super::{
    raw::{ButtonState, LinuxKeycode, RawSeatEvent, RawSeatEventKind},
    state::ConsumedShortcutKeys,
};
use crate::{ActiveBackend, WeldAppExt};
use weld_core::runtime::{HostCommand, OutputScaleAdjustment};

#[derive(Actionlike, Clone, Copy, Debug, Eq, Hash, PartialEq, Reflect)]
pub(super) enum GlobalAction {
    Terminal,
    Firefox,
    Blender,
    IncreaseScale,
    DecreaseScale,
    Exit,
}

#[derive(Clone, Copy)]
enum GlobalShortcutCommand {
    Launch(&'static str),
    AdjustOutputScale(OutputScaleAdjustment),
    Exit,
}

#[derive(Clone, Copy)]
struct GlobalShortcutDefinition {
    action: GlobalAction,
    trigger: LinuxKeycode,
    trigger_key: KeyCode,
    shift: bool,
    drm_only: bool,
    command: GlobalShortcutCommand,
}

impl GlobalShortcutDefinition {
    fn binding(self) -> ButtonlikeChord {
        let binding = ButtonlikeChord::modified(ModifierKey::Super, self.trigger_key);
        if self.shift {
            binding.with(ModifierKey::Shift)
        } else {
            binding
        }
    }

    fn host_command(self) -> HostCommand {
        match self.command {
            GlobalShortcutCommand::Launch(program) => HostCommand::Launch {
                program: program.into(),
                arguments: Vec::new(),
            },
            GlobalShortcutCommand::AdjustOutputScale(adjustment) => {
                HostCommand::AdjustOutputScale(adjustment)
            }
            GlobalShortcutCommand::Exit => HostCommand::Exit,
        }
    }
}

const GLOBAL_SHORTCUTS: [GlobalShortcutDefinition; 6] = [
    GlobalShortcutDefinition {
        action: GlobalAction::Terminal,
        trigger: LinuxKeycode(28),
        trigger_key: KeyCode::Enter,
        shift: false,
        drm_only: false,
        command: GlobalShortcutCommand::Launch("foot"),
    },
    GlobalShortcutDefinition {
        action: GlobalAction::Firefox,
        trigger: LinuxKeycode(33),
        trigger_key: KeyCode::KeyF,
        shift: false,
        drm_only: false,
        command: GlobalShortcutCommand::Launch("firefox"),
    },
    GlobalShortcutDefinition {
        action: GlobalAction::Blender,
        trigger: LinuxKeycode(48),
        trigger_key: KeyCode::KeyB,
        shift: false,
        drm_only: false,
        command: GlobalShortcutCommand::Launch("blender"),
    },
    GlobalShortcutDefinition {
        action: GlobalAction::IncreaseScale,
        trigger: LinuxKeycode(13),
        trigger_key: KeyCode::Equal,
        shift: false,
        drm_only: true,
        command: GlobalShortcutCommand::AdjustOutputScale(OutputScaleAdjustment::Increase),
    },
    GlobalShortcutDefinition {
        action: GlobalAction::DecreaseScale,
        trigger: LinuxKeycode(12),
        trigger_key: KeyCode::Minus,
        shift: false,
        drm_only: true,
        command: GlobalShortcutCommand::AdjustOutputScale(OutputScaleAdjustment::Decrease),
    },
    GlobalShortcutDefinition {
        action: GlobalAction::Exit,
        trigger: LinuxKeycode(1),
        trigger_key: KeyCode::Escape,
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
        let input_map = global_shortcut_map(&shortcuts);
        app.add_plugins(InputManagerPlugin::<GlobalAction>::default())
            .init_resource::<GlobalHostCommands>()
            .insert_resource(RawGlobalShortcutState {
                shortcuts: shortcuts.clone(),
                pressed: HashSet::new(),
            });
        app.world_mut().spawn(input_map);
    }
}

fn global_shortcut_map(shortcuts: &[GlobalShortcutDefinition]) -> InputMap<GlobalAction> {
    let mut input_map = InputMap::default();
    for shortcut in shortcuts {
        input_map.insert(shortcut.action, shortcut.binding());
    }
    input_map
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
                .map(GlobalShortcutDefinition::host_command)
        }
    };

    let consumed = world
        .get_resource::<ConsumedShortcutKeys>()
        .is_some_and(|consumed| consumed.0.contains(keycode));
    if let Some(command) = command {
        if let Some(mut consumed) = world.get_resource_mut::<ConsumedShortcutKeys>() {
            consumed.0.insert(*keycode);
        }
        if let Some(mut commands) = world.get_resource_mut::<GlobalHostCommands>() {
            commands.0.push_back(command);
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
