//! i3 interaction policies and Sway configuration interpretation for Weld.
//!
//! The focus policy traverses the shared tiling tree and submits ordinary
//! managed-window commands. Window identity, node history and tree mutation remain
//! owned by `weld-window` and `weld-tile`.

pub mod config;
mod focus;
mod layout;
mod movement;
pub use layout::{I3LayoutRequest, LayoutChoice};
mod sticky;
pub use sticky::I3StickyRequest;
mod tree;
pub mod window_rules;
pub mod workspace;

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{event::Event, resource::Resource, schedule::IntoScheduleConfigs},
};
use weld_tile::{Direction, TileSystems};

/// Directional navigation through the current i3 layout and focus history.
#[derive(Event, Clone, Copy, Debug)]
pub struct I3FocusRequest(pub Direction);

/// Move the selected leaf through i3's split-tree insertion rules.
#[derive(Event, Clone, Copy, Debug)]
pub struct I3MoveRequest(pub Direction);

/// Select the most recently focused window in the other layout plane.
#[derive(Event, Clone, Copy, Debug)]
pub struct I3FocusModeToggle;

/// Select an ancestor split or its last-selected direct child.
#[derive(Event, Clone, Copy, Debug, PartialEq)]
pub enum I3FocusHierarchy {
    Parent,
    Child,
}

/// Where directional navigation may wrap after reaching a split edge.
#[derive(Resource, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FocusWrapping {
    No,
    #[default]
    Yes,
    Force,
    /// Keeps navigation within the current workspace. Directional output
    /// traversal remains a follow-up, so currently this matches [`Self::Yes`].
    Workspace,
}

/// Installs i3 navigation and close-focus restoration over the shared tree.
pub struct I3QuirksPlugin;

impl Plugin for I3QuirksPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FocusWrapping>()
            .init_resource::<window_rules::WindowRules>()
            .init_resource::<workspace::WorkspaceSettings>()
            .init_resource::<workspace::PreviousWorkspace>()
            .add_observer(workspace::request)
            .add_observer(workspace::activate_existing)
            .add_observer(workspace::apply_resolved)
            .add_observer(workspace::finish_switch)
            .add_observer(sticky::request)
            .add_observer(workspace::remember)
            .add_observer(workspace::bootstrap)
            .add_observer(workspace::after_move)
            .init_resource::<focus::FocusPath>()
            .add_observer(focus::navigate)
            .add_observer(layout::request)
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
            window_rules::apply
                .after(weld_window::WindowSystems::Admission)
                .before(weld_window::WindowSystems::PresentationRevoke),
        );
        app.add_systems(
            PreUpdate,
            workspace::prepare
                .before(TileSystems::RecoverFocus)
                .in_set(weld_window::WindowSystems::Management),
        );
        app.add_systems(
            PreUpdate,
            workspace::reap
                .after(TileSystems::Layout)
                .in_set(weld_window::WindowSystems::Management),
        );
    }
}
