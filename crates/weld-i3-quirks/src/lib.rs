//! i3 interaction policies and Sway configuration interpretation for Weld.
//!
//! The focus policy traverses the shared tiling tree and submits ordinary
//! managed-window commands. Window identity, node history and tree mutation remain
//! owned by `weld-window` and `weld-tile`.

use weld_tile_config::Direction;
use weld_tile_config::{FocusHierarchy, FocusWrapping};
mod focus;
mod layout;
mod movement;
mod resize;
mod sticky;
pub use sticky::I3StickyRequest;
mod tree;
pub mod workspace;

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{event::Event, schedule::IntoScheduleConfigs},
};
use weld_tile::TileSystems;

/// Directional navigation through the current i3 layout and focus history.
#[derive(Event, Clone, Copy, Debug)]
pub struct I3FocusRequest(pub Direction);

/// Move the selected leaf through i3's split-tree insertion rules.
#[derive(Event, Clone, Copy, Debug)]
pub struct I3MoveRequest(pub Direction);

/// Select the most recently focused window in the other layout plane.
#[derive(Event, Clone, Copy, Debug)]
pub struct I3FocusModeToggle;

/// Installs i3 navigation and close-focus restoration over the shared tree.
pub struct I3QuirksPlugin;

impl Plugin for I3QuirksPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FocusWrapping>()
            .init_resource::<weld_window_config::WorkspaceSettings>()
            .init_resource::<workspace::PreviousWorkspace>()
            .add_observer(workspace::request)
            .add_observer(workspace::move_explicit)
            .add_observer(workspace::activate_existing)
            .add_observer(workspace::apply_resolved)
            .add_observer(workspace::finish_switch)
            .add_observer(sticky::request)
            .add_observer(workspace::remember)
            .add_observer(workspace::renamed)
            .add_observer(workspace::bootstrap)
            .add_observer(workspace::after_move)
            .init_resource::<focus::FocusPath>()
            .add_observer(focus::navigate)
            .add_observer(layout::request)
            .add_observer(resize::request)
            .add_observer(focus::mode_toggle)
            .add_observer(focus::hierarchy)
            .add_observer(focus::remember_focus)
            .add_observer(focus::tree_changed)
            .add_observer(movement::move_focused)
            .add_observer(movement::after_wrap)
            .add_observer(movement::cleanup)
            .add_systems(PreUpdate, focus::recover.in_set(TileSystems::RecoverFocus))
            .add_systems(PreUpdate, focus::refresh_path.in_set(TileSystems::Layout));
        app.add_systems(
            PreUpdate,
            workspace::prepare
                .before(weld_window::rules::WindowRuleSystems::Evaluate)
                .in_set(weld_window::WindowSystems::AdmissionPolicy),
        );
        app.add_systems(
            PreUpdate,
            workspace::reap
                .after(TileSystems::Layout)
                .in_set(weld_window::WindowSystems::Management),
        );
    }
}
