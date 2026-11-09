//! Window behavior preferences and configurable operations.

use weld_input_config::PointerShortcutModifiers;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FullscreenMode {
    /// Fill the workspace's output, retaining desktop overlay access.
    Normal,
    /// Give this application the output, including its ordinary overlay/input plane.
    /// Compositor escape shortcuts remain available.
    Exclusive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FullscreenAction {
    Enable(FullscreenMode),
    Toggle(FullscreenMode),
    Disable,
}

/// Identifier captured from the current occupant for exact replacement matching.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PersistenceMatch {
    /// Native app ID, or X11 class and instance; title is the final fallback.
    #[default]
    Auto,
    AppId,
    Class,
    Instance,
    Title,
}

/// Enable captures the occupant identity once; toggle enables or disables retention.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceAction {
    Enable(PersistenceMatch),
    Disable,
    Toggle(PersistenceMatch),
}

/// Live compositor pointer chords and focus policy, configured by the distribution.
#[cfg_attr(feature = "ecs", derive(bevy_ecs::resource::Resource))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowPointerSettings {
    pub modifier: Option<PointerShortcutModifiers>,
    /// Focus a window when mouse motion enters it, preserving its stacking order.
    pub focus_follows_mouse: bool,
}

mod workspace;
pub use workspace::{
    WorkspaceAssignment, WorkspaceCommand, WorkspaceNavigation, WorkspaceSettings, WorkspaceTarget,
};
mod rules;
pub use rules::{RuleDefinition, RuleEffect, WindowMatchField};
