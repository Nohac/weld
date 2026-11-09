//! Native configuration assembly for the weldwm distribution.
//!
//! Feature crates own settings and operations. This assembly owns cross-feature
//! actions, modes and startup effects; no frontend or runtime resource is required.

use weld_config::{ConfigWarning, Located};
use weld_input_config::{CursorThemeSettings, KeymapConfig, PointerDeviceSettings, Shortcut};
use weld_output::OutputSettings;
use weld_ssd_config::{BorderStyle, DecorationSettings};
use weld_tile_config::{
    Direction, FocusHierarchy, FocusPlaneRequest, FocusWrapping, LayoutRequest, ResizeRequest,
    TileOperation, TileSettings,
};
use weld_window_config::{
    FullscreenAction, PersistenceAction, RuleDefinition, WindowPointerSettings, WorkspaceCommand,
    WorkspaceSettings, WorkspaceTarget,
};

#[derive(Clone, Debug, PartialEq)]
pub enum Action<Extension = ()> {
    Focus(Direction),
    Move(Direction),
    Workspace(WorkspaceCommand),
    Tile(TileOperation),
    Layout(LayoutRequest),
    Floating(Option<bool>),
    Position([f32; 2]),
    FocusPlane(FocusPlaneRequest),
    FocusHierarchy(FocusHierarchy),
    Sticky(Option<bool>),
    Border(Option<BorderStyle>),
    Persistent(PersistenceAction),
    Fullscreen(FullscreenAction),
    Reload,
    Mode(String),
    Resize(ResizeRequest),
    Exec(String),
    Exit,
    Extension(Extension),
}

#[derive(Clone, Debug)]
pub struct Configuration<Extension = ()> {
    pub tiling: TileSettings,
    pub focus_wrapping: FocusWrapping,
    pub bindings: Vec<(Shortcut, Action<Extension>)>,
    pub modes: Vec<BindingMode<Extension>>,
    pub keymap: Option<KeymapConfig>,
    pub pointer_devices: PointerDeviceSettings,
    pub cursor: Option<CursorThemeSettings>,
    pub outputs: OutputSettings,
    pub workspaces: WorkspaceSettings,
    pub startup: Vec<StartupCommand>,
    pub pointer: WindowPointerSettings,
    pub decorations: DecorationSettings,
    pub window_rules: Vec<Located<RuleDefinition<RuleAction>>>,
    pub warnings: Vec<ConfigWarning>,
}

#[derive(Clone, Debug)]
pub struct BindingMode<Extension = ()> {
    pub name: String,
    pub pango_markup: bool,
    pub bindings: Vec<(Shortcut, Action<Extension>)>,
}

/// A validated shell command and its configuration-load execution policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupCommand {
    pub command: String,
    pub on_reload: bool,
}

impl<Extension> Default for Configuration<Extension> {
    fn default() -> Self {
        Self {
            tiling: TileSettings::default(),
            focus_wrapping: FocusWrapping::default(),
            bindings: Vec::new(),
            modes: Vec::new(),
            keymap: None,
            pointer_devices: Default::default(),
            cursor: None,
            outputs: Default::default(),
            workspaces: WorkspaceSettings::default(),
            startup: Vec::new(),
            pointer: WindowPointerSettings {
                focus_follows_mouse: true,
                ..Default::default()
            },
            decorations: DecorationSettings::default(),
            window_rules: Default::default(),
            warnings: Vec::new(),
        }
    }
}

/// Supported cross-feature actions for weldwm window rules.
#[derive(Clone, Debug, PartialEq)]
pub enum RuleAction {
    Persistent(PersistenceAction),
    Border(BorderStyle),
    Floating(bool),
    Workspace(WorkspaceTarget),
    Position([f32; 2]),
    Fullscreen(FullscreenAction),
    Sticky(Option<bool>),
}
