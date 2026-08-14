//! Shared state for advancing and targeting Bevy's compositor composition.

use bevy::{
    app::{App, Plugin},
    ecs::{resource::Resource, system::Res, world::World},
};

/// Installs the host-controlled composition advance state.
pub struct CompositionPlugin;

impl Plugin for CompositionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CompositionAdvance>();
    }
}

/// Whether this main-world update is paired with render extraction.
#[derive(Resource)]
pub struct CompositionAdvance(bool);

impl Default for CompositionAdvance {
    fn default() -> Self {
        Self(true)
    }
}

pub fn composition_advance_requested(advance: Res<CompositionAdvance>) -> bool {
    advance.0
}

pub(crate) fn set_composition_advance(world: &mut World, enabled: bool) {
    if let Some(mut advance) = world.get_resource_mut::<CompositionAdvance>() {
        advance.0 = enabled;
    }
}

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn set_composition_advance_for_test(world: &mut World, enabled: bool) {
    set_composition_advance(world, enabled);
}
