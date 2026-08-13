//! Leafwing-backed global shortcuts and host commands.

use std::collections::{HashSet, VecDeque};

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        component::Component,
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Query, Res, ResMut},
        world::World,
    },
    input::keyboard::KeyCode,
    prelude::Reflect,
};
use leafwing_input_manager::{
    plugin::{InputManagerPlugin, InputManagerSystem},
    prelude::{ActionState, Actionlike, ButtonlikeChord, InputMap, ModifierKey},
};

use super::{
    InputSystems,
    raw::{ButtonState, LinuxKeycode, RawSeatEvent, RawSeatEventKind},
    state::{ConsumedShortcutKeys, PendingSeatInput},
};
use crate::{ActiveBackend, WeldAppExt};
use weld_core::runtime::{HostCommand, OutputScaleAdjustment};

#[derive(Actionlike, Clone, Copy, Debug, Eq, Hash, PartialEq, Reflect)]
enum GlobalAction {
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

#[derive(Component)]
struct GlobalShortcutBindings(Vec<GlobalShortcutDefinition>);

#[derive(Resource, Default)]
struct GlobalHostCommands(VecDeque<HostCommand>);

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
            .add_systems(
                PreUpdate,
                collect_global_shortcuts
                    .after(InputManagerSystem::Update)
                    .before(InputSystems::Resolve),
            );
        app.world_mut()
            .spawn((GlobalShortcutBindings(shortcuts), input_map));
    }
}

fn global_shortcut_map(shortcuts: &[GlobalShortcutDefinition]) -> InputMap<GlobalAction> {
    let mut input_map = InputMap::default();
    for shortcut in shortcuts {
        input_map.insert(shortcut.action, shortcut.binding());
    }
    input_map
}

fn collect_global_shortcuts(
    bindings: Query<(&ActionState<GlobalAction>, &GlobalShortcutBindings)>,
    pending: Res<PendingSeatInput>,
    mut commands: ResMut<GlobalHostCommands>,
    mut consumed: ResMut<ConsumedShortcutKeys>,
) {
    let Some((actions, shortcuts)) = bindings.iter().next() else {
        return;
    };
    for shortcut in &shortcuts.0 {
        if actions.just_pressed(&shortcut.action)
            && contains_key_press(&pending.0, shortcut.trigger)
        {
            consumed.0.insert(shortcut.trigger);
            commands.0.push_back(shortcut.host_command());
        }
    }
}

fn contains_key_press(events: &VecDeque<RawSeatEvent>, trigger: LinuxKeycode) -> bool {
    events.iter().any(|event| {
        matches!(
            &event.event,
            RawSeatEventKind::Keyboard {
                keycode,
                state: ButtonState::Pressed,
                ..
            } if *keycode == trigger
        )
    })
}

pub(crate) fn take_host_commands(world: &mut World) -> Vec<HostCommand> {
    world
        .get_resource_mut::<GlobalHostCommands>()
        .map(|mut commands| commands.0.drain(..).collect())
        .unwrap_or_default()
}

pub(super) fn consume_shortcut_event(
    consumed: &mut HashSet<LinuxKeycode>,
    event: &RawSeatEvent,
) -> bool {
    match &event.event {
        RawSeatEventKind::Keyboard { keycode, state, .. } if consumed.contains(keycode) => {
            if *state == ButtonState::Released {
                consumed.remove(keycode);
            }
            true
        }
        RawSeatEventKind::HostFocusLost => {
            consumed.clear();
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::consume_shortcut_event;
    use crate::input::raw::{ButtonState, LinuxKeycode, RawSeatEvent, RawSeatEventKind};

    #[test]
    fn host_focus_loss_clears_a_consumed_shortcut_release() {
        let mut consumed = HashSet::from([LinuxKeycode(33)]);
        assert!(!consume_shortcut_event(
            &mut consumed,
            &RawSeatEvent::new(RawSeatEventKind::HostFocusLost, 20),
        ));
        assert!(consumed.is_empty());
        assert!(!consume_shortcut_event(
            &mut consumed,
            &RawSeatEvent::new(
                RawSeatEventKind::Keyboard {
                    keycode: LinuxKeycode(33),
                    logical_key: None,
                    state: ButtonState::Released,
                },
                21,
            ),
        ));
    }
}
