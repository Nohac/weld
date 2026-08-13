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
#[cfg(feature = "profiling-tracy")]
const PROFILE_TARGET: &str = "weld_profile";

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
    initialize_tracing()?;

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

/// Installs Weld's process-wide tracing subscriber.
///
/// This must complete before constructing [`WeldApp`]: Bevy's Tracy GPU setup
/// expects the Tracy client installed by the subscriber layer to be running.
fn initialize_tracing() -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    initialize_tracing_subscriber(filter)
}

#[cfg(not(feature = "profiling-tracy"))]
fn initialize_tracing_subscriber(filter: tracing_subscriber::EnvFilter) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()
        .map_err(|error| anyhow!("failed to initialize tracing: {error}"))
}

#[cfg(feature = "profiling-tracy")]
fn initialize_tracing_subscriber(filter: tracing_subscriber::EnvFilter) -> Result<()> {
    use tracing_subscriber::{
        Layer as _,
        filter::{FilterExt as _, FilterFn, LevelFilter, Targets},
        layer::SubscriberExt as _,
        util::SubscriberInitExt as _,
    };

    let formatted_filter = filter.and(FilterFn::new(|metadata| {
        metadata.fields().field("tracy.frame_mark").is_none()
    }));
    let formatted_logs = tracing_subscriber::fmt::layer().with_filter(formatted_filter);
    let tracy_filter = Targets::new()
        .with_default(LevelFilter::INFO)
        .with_target(PROFILE_TARGET, LevelFilter::TRACE);
    tracing_subscriber::registry()
        .with(formatted_logs)
        .with(tracing_tracy::TracyLayer::default().with_filter(tracy_filter))
        .try_init()
        .map_err(|error| anyhow!("failed to initialize tracing: {error}"))?;
    tracing::warn!(
        "Tracy profiling is active; capture memory grows while no Tracy client is connected"
    );
    Ok(())
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
