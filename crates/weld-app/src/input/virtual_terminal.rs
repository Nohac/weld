//! DRM-only virtual-terminal switching shortcuts.

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Res, ResMut},
        world::World,
    },
    input::{ButtonInput, InputSystems as BevyInputSystems, keyboard::KeyCode},
};

use crate::{ActiveBackend, WeldAppExt};

use super::{
    InputSystems,
    raw::{ButtonState, RawSeatEventKind},
    state::{ConsumedShortcutKeys, PendingSeatInput},
};

const FIRST_FUNCTION_KEY: u32 = 59;
const LAST_FUNCTION_KEY: u32 = 68;

#[derive(Resource, Default)]
pub(crate) struct VirtualTerminalSwitchRequest(Option<i32>);

/// Enables virtual-terminal shortcuts when added to a DRM-backed [`crate::WeldApp`].
///
/// Add this after [`crate::WeldAppBuilder::build`] has inserted the resolved
/// [`ActiveBackend`]. The plugin is intentionally inactive for other Bevy apps
/// and for Weld's nested backend.
pub struct VirtualTerminalShortcutPlugin;

impl Plugin for VirtualTerminalShortcutPlugin {
    fn build(&self, app: &mut App) {
        if app.backend() != Some(ActiveBackend::Drm) {
            return;
        }
        app.init_resource::<VirtualTerminalSwitchRequest>()
            .add_systems(
                PreUpdate,
                collect_virtual_terminal_shortcuts
                    .after(BevyInputSystems)
                    .before(InputSystems::Resolve),
            );
    }
}

fn collect_virtual_terminal_shortcuts(
    keys: Res<ButtonInput<KeyCode>>,
    pending: Res<PendingSeatInput>,
    mut request: ResMut<VirtualTerminalSwitchRequest>,
    mut consumed: ResMut<ConsumedShortcutKeys>,
) {
    let control_pressed = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    let alt_pressed = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    if !control_pressed || !alt_pressed {
        return;
    }

    for event in &pending.0 {
        if let RawSeatEventKind::Keyboard {
            keycode,
            state: ButtonState::Pressed,
            ..
        } = &event.event
            && (FIRST_FUNCTION_KEY..=LAST_FUNCTION_KEY).contains(&keycode.0)
        {
            let Ok(virtual_terminal) = i32::try_from(keycode.0 - FIRST_FUNCTION_KEY + 1) else {
                continue;
            };
            consumed.0.insert(*keycode);
            request.0 = Some(virtual_terminal);
        }
    }
}

pub(crate) fn take_virtual_terminal_switch_request(world: &mut World) -> Option<i32> {
    world
        .get_resource_mut::<VirtualTerminalSwitchRequest>()
        .and_then(|mut request| request.0.take())
}
