//! Input sources, Bevy/Leafwing projection, and client-surface routing.
//!
//! Host-specific sources emit owned raw::RawSeatEvent values. The input
//! bridge projects each batch into standard Bevy and Leafwing input in First,
//! then resolves Bevy picking and emits protocol-neutral client effects in
//! PreUpdate. Smithay resources never enter the ECS world.

mod projection;
mod routing;
mod shortcuts;
mod state;
mod virtual_terminal;

pub(crate) mod raw {
    pub use weld_core::input::*;
}

use bevy::{
    app::{App, Plugin},
    camera::NormalizedRenderTarget,
    ecs::schedule::SystemSet,
};

pub(crate) use projection::enqueue_raw_input;
pub(crate) use routing::take_input_effects;
pub use shortcuts::GlobalShortcutPlugin;
pub(crate) use shortcuts::take_host_commands;
pub(crate) use state::{projected_pointer_position, set_input_update_time};
pub use virtual_terminal::VirtualTerminalShortcutPlugin;
pub(crate) use virtual_terminal::take_virtual_terminal_switch_request;
pub use weld_core::input::{SeatInputEffect, SeatInputEffectKind, SurfaceHit};

#[derive(Clone, Debug, Eq, Hash, PartialEq, SystemSet)]
pub(crate) enum InputSystems {
    Resolve,
}

pub(crate) struct InputBridgePlugin {
    target: NormalizedRenderTarget,
}

impl InputBridgePlugin {
    /// Build the input bridge for Weld's manual composition target.
    ///
    /// Input-only host advances no longer imply a rendered composition.
    /// Picking observers and action systems that mutate visuals must emit a
    /// Bevy RequestRedraw message.
    pub(crate) const fn new(target: NormalizedRenderTarget) -> Self {
        Self { target }
    }
}

impl Plugin for InputBridgePlugin {
    fn build(&self, app: &mut App) {
        state::register(app);
        projection::register(app, self.target.clone());
        routing::register(app);
    }
}

#[cfg(test)]
mod tests;
