//! Shared state passed between input pipeline stages.

use std::collections::{HashSet, VecDeque};

use bevy::{
    app::App,
    ecs::{resource::Resource, world::World},
};

use super::raw::{InputPosition, LinuxKeycode, RawSeatEvent};

pub(super) fn register(app: &mut App) {
    app.init_resource::<PendingSeatInput>()
        .init_resource::<ProjectedPointerState>()
        .init_resource::<InputUpdateTime>()
        .init_resource::<ConsumedShortcutKeys>();
}

#[derive(Resource, Default)]
pub(super) struct PendingSeatInput(pub(super) VecDeque<RawSeatEvent>);

/// Projection and protocol routing deliberately own separate instances: the
/// former advances in First, while the latter replays lossless events in
/// PreUpdate. Merging them collapses every batch to its final pointer state.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct PointerPositionState {
    pub(super) host_position: Option<InputPosition>,
    pub(super) last_known_position: InputPosition,
}

impl PointerPositionState {
    pub(super) fn apply(&mut self, position: InputPosition) {
        self.host_position = Some(position);
        self.last_known_position = position;
    }

    /// Clear host presence without discarding the location used for cancel or
    /// position-less button and axis events.
    pub(super) fn clear_host(&mut self) {
        self.host_position = None;
    }
}

#[derive(Resource, Default)]
pub(super) struct ProjectedPointerState(pub(super) PointerPositionState);

#[derive(Resource, Default)]
pub(super) struct InputUpdateTime(pub(super) u32);

#[derive(Resource, Default)]
pub(super) struct ConsumedShortcutKeys(pub(super) HashSet<LinuxKeycode>);

pub(crate) fn set_input_update_time(world: &mut World, time: u32) {
    if let Some(mut update_time) = world.get_resource_mut::<InputUpdateTime>() {
        update_time.0 = time;
    }
}
