//! Input sources, Bevy/Leafwing projection, and client-surface routing.
//!
//! Host-specific sources emit owned raw::RawSeatEvent values. The input
//! bridge projects each refresh-paced batch into standard Bevy and Leafwing
//! input in First, then publishes the picked client target in PreUpdate. Core
//! retains that target and forwards unconsumed raw events to Smithay at input
//! pace. Smithay resources never enter the application world.

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

pub use projection::TouchpadGesture;
pub(crate) use projection::enqueue_raw_input;
pub(crate) use routing::take_input_effects;
pub use shortcuts::GlobalShortcutPlugin;
pub(crate) use shortcuts::{filter_global_shortcut_event, take_host_commands};
pub(crate) use state::set_input_update_time;
pub use virtual_terminal::VirtualTerminalShortcutPlugin;
pub(crate) use virtual_terminal::{
    filter_virtual_terminal_event, take_virtual_terminal_switch_request,
};
pub use weld_core::input::{
    InputDelta, PointerGesture, PointerGestureKind, SeatInputEffect, SeatInputEffectKind,
    SurfaceHit, TouchpadHold, TouchpadPinch, TouchpadSwipe,
};

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
    /// Raw input is buffered until the next refresh-paced application frame.
    /// Client delivery remains independent of that frame through the target
    /// most recently published by picking.
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
