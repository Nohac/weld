//! DRM-only virtual-terminal switching shortcuts.

use bevy::{
    app::{App, Plugin},
    ecs::{resource::Resource, world::World},
};

use crate::{ActiveBackend, WeldAppExt};

use super::{
    raw::{ButtonState, LinuxKeycode, RawSeatEvent, RawSeatEventKind},
    state::ConsumedShortcutKeys,
};

const FIRST_FUNCTION_KEY: u32 = 59;
const LAST_FUNCTION_KEY: u32 = 68;

#[derive(Resource, Default)]
pub(crate) struct VirtualTerminalSwitchRequest(Option<i32>);

#[derive(Resource, Default)]
struct RawVirtualTerminalState {
    pressed: std::collections::HashSet<LinuxKeycode>,
}

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
            .init_resource::<RawVirtualTerminalState>();
    }
}

pub(crate) fn filter_virtual_terminal_event(world: &mut World, event: &RawSeatEvent) -> bool {
    let RawSeatEventKind::Keyboard { keycode, state, .. } = &event.event else {
        if matches!(event.event, RawSeatEventKind::HostFocusLost)
            && let Some(mut keys) = world.get_resource_mut::<RawVirtualTerminalState>()
        {
            keys.pressed.clear();
            if let Some(mut consumed) = world.get_resource_mut::<ConsumedShortcutKeys>() {
                consumed.0.clear();
            }
        }
        return false;
    };
    let should_switch = {
        let Some(mut keys) = world.get_resource_mut::<RawVirtualTerminalState>() else {
            return false;
        };
        let newly_pressed = match state {
            ButtonState::Pressed => keys.pressed.insert(*keycode),
            ButtonState::Released => {
                keys.pressed.remove(keycode);
                false
            }
        };
        let control = [29, 97]
            .into_iter()
            .any(|code| keys.pressed.contains(&LinuxKeycode(code)));
        let alt = [56, 100]
            .into_iter()
            .any(|code| keys.pressed.contains(&LinuxKeycode(code)));
        newly_pressed
            && control
            && alt
            && (FIRST_FUNCTION_KEY..=LAST_FUNCTION_KEY).contains(&keycode.0)
    };

    if should_switch {
        if let Ok(virtual_terminal) = i32::try_from(keycode.0 - FIRST_FUNCTION_KEY + 1)
            && let Some(mut request) = world.get_resource_mut::<VirtualTerminalSwitchRequest>()
        {
            request.0 = Some(virtual_terminal);
        }
        if let Some(mut consumed) = world.get_resource_mut::<ConsumedShortcutKeys>() {
            consumed.0.insert(*keycode);
        }
        return true;
    }

    let consumed = world
        .get_resource::<ConsumedShortcutKeys>()
        .is_some_and(|consumed| consumed.0.contains(keycode));
    if consumed
        && *state == ButtonState::Released
        && let Some(mut consumed) = world.get_resource_mut::<ConsumedShortcutKeys>()
    {
        consumed.0.remove(keycode);
    }
    consumed
}

pub(crate) fn take_virtual_terminal_switch_request(world: &mut World) -> Option<i32> {
    world
        .get_resource_mut::<VirtualTerminalSwitchRequest>()
        .and_then(|mut request| request.0.take())
}
