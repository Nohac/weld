//! Standard Weld compositor distribution and backend selection.

use std::{ffi::OsString, path::PathBuf};

use anyhow::{Result, anyhow};
use bevy::{
    app::{App, Plugin, Startup},
    ecs::system::Commands,
    prelude::{
        AlignItems, BackgroundColor, BorderRadius, Color, GlobalZIndex, Node, PositionType, UiRect,
        px,
    },
    scene::{CommandsSceneExt, bsn},
    text::{FontSourceTemplate, TextColor, TextFont},
    ui::widget::{Text, TextShadow},
};
use clap::{Parser, ValueEnum};
use weld_app::{
    Backend, WeldApp,
    input::{GlobalShortcutPlugin, VirtualTerminalShortcutPlugin},
};
use weld_window::DefaultWindowPlugin;

const DEFAULT_REMOTE_ADDRESS: &str = "127.0.0.1:15702";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum BackendKind {
    #[default]
    Auto,
    Nested,
    Drm,
}

impl BackendKind {
    fn as_backend(self) -> Backend {
        match self {
            Self::Auto => Backend::Auto,
            Self::Nested => Backend::Nested,
            Self::Drm => Backend::Drm,
        }
    }
}

#[derive(Parser)]
#[command(
    version,
    about = "Bevy-native Wayland compositor",
    trailing_var_arg = true
)]
pub struct AppArguments {
    /// Host backend. Auto uses a nested host when available and DRM on a TTY.
    #[arg(long, value_enum, default_value_t)]
    pub(crate) backend: BackendKind,

    /// Enable the restricted Bevy Remote Protocol endpoint.
    #[arg(
        long,
        value_name = "HOST:PORT",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = DEFAULT_REMOTE_ADDRESS
    )]
    pub(crate) remote_debug: Option<String>,

    /// Capture the first settled composition and exit.
    #[arg(long, value_name = "PATH")]
    pub(crate) screenshot: Option<PathBuf>,

    /// Optional client program followed by its arguments.
    #[arg(value_name = "CLIENT_AND_ARGS", allow_hyphen_values = true)]
    pub(crate) client: Vec<OsString>,
}

pub fn run(arguments: AppArguments) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init()
        .map_err(|error| anyhow!("failed to initialize tracing: {error}"))?;

    let mut app = WeldApp::builder()
        .backend(arguments.backend.as_backend())
        .launch(arguments.client)
        .screenshot(arguments.screenshot)
        .remote_debug(arguments.remote_debug)
        .build()?;
    app.add_plugins((
        DefaultWindowPlugin,
        GlobalShortcutPlugin,
        VirtualTerminalShortcutPlugin,
        DistributionOverlayPlugin,
    ));
    app.run()
}

pub fn run_from_env() -> Result<()> {
    run(AppArguments::parse())
}

struct DistributionOverlayPlugin;

impl Plugin for DistributionOverlayPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_distribution_overlay);
    }
}

fn spawn_distribution_overlay(mut commands: Commands) {
    commands.spawn_scene(bsn! {
        Node {
            position_type: PositionType::Absolute,
            top: px(24),
            right: px(24),
            width: px(240),
            height: px(88),
            padding: UiRect::all(px(16)),
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(18)),
        }
        Text("Weld Master")
        TextFont {
            font: FontSourceTemplate::Monospace,
            font_size: px(20.0),
        }
        TextColor(Color::srgb(0.9, 0.9, 0.9))
        TextShadow
        BackgroundColor(Color::srgba(0.08, 0.34, 0.48, 0.82))
        GlobalZIndex(weld_app::layer::SHELL_Z_INDEX)
    });
}
