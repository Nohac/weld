//! Standard Weld compositor distribution and backend selection.

mod arguments;
mod overlay;
mod telemetry;

use anyhow::Result;
use clap::Parser;
use overlay::DistributionOverlayPlugin;
use weld_app::{
    WeldApp,
    input::{GlobalShortcutPlugin, VirtualTerminalShortcutPlugin},
};
use weld_float::FloatPlugin;
use weld_ssd::SsdPlugin;
use weld_window::WindowPlugin;
use weld_window_ui::WindowUiPlugin;

pub use arguments::{AppArguments, BackendKind};

pub fn run(arguments: AppArguments) -> Result<()> {
    telemetry::initialize()?;

    let mut app = WeldApp::builder()
        .backend(arguments.backend.as_backend())
        .launch(arguments.client)
        .screenshot(arguments.screenshot)
        .remote_debug(arguments.remote_debug)
        .scale(arguments.scale)
        .build()?;
    app.add_plugins((
        WindowPlugin,
        WindowUiPlugin,
        SsdPlugin,
        FloatPlugin,
        GlobalShortcutPlugin,
        VirtualTerminalShortcutPlugin,
        DistributionOverlayPlugin,
    ));
    app.run()
}

pub fn run_from_env() -> Result<()> {
    run(AppArguments::parse())
}
