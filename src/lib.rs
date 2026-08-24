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
use weld_hoist::{HoistPlugin, HoistTransport, loopback_registration};
use weld_hoist_local::{
    LocalPacketConnection, LocalPacketListener, LocalPeerRole, local_destination_registration,
    local_source_registration,
};
use weld_ssd::SsdPlugin;
use weld_window::WindowPlugin;
use weld_window_ui::WindowUiPlugin;

pub use arguments::{AppArguments, BackendKind};

pub fn run(arguments: AppArguments) -> Result<()> {
    telemetry::initialize()?;

    let local_transport = if let Some(path) = &arguments.hoist_listen {
        let listener = LocalPacketListener::bind(path, LocalPeerRole::Source)?;
        Some((LocalPeerRole::Source, listener.accept_blocking()?))
    } else if let Some(path) = &arguments.hoist_connect {
        Some((
            LocalPeerRole::Destination,
            LocalPacketConnection::connect(path, LocalPeerRole::Destination)?,
        ))
    } else {
        None
    };

    let mut app = WeldApp::builder()
        .backend(arguments.backend.as_backend())
        .launch(arguments.client)
        .screenshot(arguments.screenshot)
        .remote_debug(arguments.remote_debug)
        .scale(arguments.scale)
        .socket_name(arguments.wayland_socket)
        .build()?;
    let mut enable_hoist_policy = true;
    match local_transport {
        Some((LocalPeerRole::Source, connection)) => {
            let (adapter, endpoint) = local_source_registration(
                connection.clone(),
                weld_core::WAYLAND_CLIENT_SOURCE,
                weld_client::ClientSourceId::new(1),
                weld_client::ClientSourceId::new(1),
            );
            app.add_client_wake_source(connection.runtime_wake_source())
                .add_client_adapter(adapter)
                .insert_resource(HoistTransport::new(endpoint));
        }
        Some((LocalPeerRole::Destination, connection)) => {
            let adapter = local_destination_registration(
                connection.clone(),
                weld_core::WAYLAND_CLIENT_SOURCE,
                weld_client::ClientSourceId::new(1),
                app.dmabuf_context(),
            );
            app.add_client_wake_source(connection.runtime_wake_source())
                .add_client_adapter(adapter);
            enable_hoist_policy = false;
        }
        None => {
            let (adapter, endpoint) = loopback_registration(
                weld_core::WAYLAND_CLIENT_SOURCE,
                weld_client::ClientSourceId::new(1),
            );
            app.add_client_adapter(adapter)
                .insert_resource(HoistTransport::new(endpoint));
        }
    }
    app.add_plugins((
        WindowPlugin,
        WindowUiPlugin,
        SsdPlugin,
        FloatPlugin,
        GlobalShortcutPlugin,
        VirtualTerminalShortcutPlugin,
        DistributionOverlayPlugin,
    ));
    if enable_hoist_policy {
        app.add_plugins(HoistPlugin);
    }
    app.run()
}

pub fn run_from_env() -> Result<()> {
    run(AppArguments::parse())
}
