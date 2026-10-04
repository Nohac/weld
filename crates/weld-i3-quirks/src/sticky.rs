//! i3 stickiness follows workspace switches on the window's current output.

use bevy::ecs::{
    entity::Entity,
    event::Event,
    observer::On,
    query::With,
    system::{Commands, Query, Res, SystemParam},
};
use weld_window::{
    FloatingWindow, FocusedWindow, ManagedWindow, StickyWindow, workspace::WorkspaceMember,
};

/// Set or toggle the focused window's retained sticky preference.
#[derive(Event, Clone, Copy, Debug)]
pub struct I3StickyRequest(pub Option<bool>);

pub(crate) fn request(
    event: On<I3StickyRequest>,
    focus: Res<FocusedWindow>,
    windows: Query<Option<&StickyWindow>, With<ManagedWindow>>,
    mut commands: Commands,
) {
    let Some(window) = focus.entity() else { return };
    let Ok(sticky) = windows.get(window) else {
        return;
    };
    if event.0.unwrap_or(sticky.is_none()) {
        commands.entity(window).insert(StickyWindow);
    } else {
        commands.entity(window).remove::<StickyWindow>();
    }
}

#[derive(SystemParam)]
pub(crate) struct StickyWindows<'w, 's> {
    pub windows: Query<'w, 's, (Entity, &'static WorkspaceMember), StickyFloat>,
    pub focus: Res<'w, FocusedWindow>,
}

type StickyFloat = (
    With<ManagedWindow>,
    With<FloatingWindow>,
    With<StickyWindow>,
);
