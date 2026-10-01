//! i3 interaction policies and Sway configuration interpretation for Weld.
//!
//! The focus policy traverses the shared tiling tree and submits ordinary
//! managed-window commands. Window identity, node history and tree mutation remain
//! owned by `weld-window` and `weld-tile`.

pub mod config;
mod focus;

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{event::Event, resource::Resource, schedule::IntoScheduleConfigs},
};
use weld_tile::{Direction, TileSystems};

/// Directional navigation through the current i3 layout and focus history.
#[derive(Event, Clone, Copy, Debug)]
pub struct I3FocusRequest(pub Direction);

/// Where directional navigation may wrap after reaching a split edge.
#[derive(Resource, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FocusWrapping {
    No,
    #[default]
    Yes,
    Force,
    /// Keeps navigation within the current workspace. With one workspace this
    /// has the same result as [`Self::Yes`].
    Workspace,
}

/// Installs i3 navigation and close-focus restoration over the shared tree.
pub struct I3QuirksPlugin;

impl Plugin for I3QuirksPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FocusWrapping>()
            .init_resource::<focus::FocusPath>()
            .add_observer(focus::navigate)
            .add_observer(focus::remember_focus)
            .add_systems(PreUpdate, focus::recover.in_set(TileSystems::RecoverFocus))
            .add_systems(PreUpdate, focus::refresh_path.in_set(TileSystems::Layout));
    }
}
