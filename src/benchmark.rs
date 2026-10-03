//! Distribution-level setup for the opt-in offscreen comparison.

use anyhow::Result;
use bevy::app::App;
use std::path::Path;
use weld_app::input::{GlobalShortcutPlugin, VirtualTerminalShortcutPlugin};
use weld_hoist::{HoistEndpointRegistry, HoistPlugin};
use weld_ssd::SsdPlugin;
use weld_tile::TilePlugin;
use weld_window::WindowPlugin;
use weld_window_ui::WindowUiPlugin;

/// Install the distribution's subscriber before the profiling renderer starts.
pub fn initialize_tracing() -> Result<()> {
    crate::telemetry::initialize()
}

/// Install Master with an explicit fixture config; startup commands are rejected
/// by the benchmark driver so application CPU stays in the separate producer.
pub fn configure(app: &mut App, config: &Path) -> Result<()> {
    app.init_resource::<HoistEndpointRegistry>();
    app.add_plugins((
        WindowPlugin,
        WindowUiPlugin,
        SsdPlugin,
        TilePlugin,
        GlobalShortcutPlugin,
        VirtualTerminalShortcutPlugin,
        crate::overlay::DistributionOverlayPlugin,
        crate::master::MasterConfigPlugin::load(config)?,
        HoistPlugin,
    ));
    Ok(())
}
