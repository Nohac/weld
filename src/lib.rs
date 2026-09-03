//! Standard Weld compositor distribution and backend selection.

mod arguments;
mod overlay;
mod telemetry;

use anyhow::{Context, Result};
use clap::Parser;
use overlay::DistributionOverlayPlugin;
use weld_app::{
    WeldApp,
    input::{GlobalShortcutPlugin, VirtualTerminalShortcutPlugin},
};
use weld_float::FloatPlugin;
use weld_hoist::{HoistEndpointRegistry, HoistPlugin, loopback_registration};
use weld_hoist_local::{
    EncodedSourceRegistrationOptions, LocalPacketConnection, LocalPacketListener, LocalPeerRole,
    LocalSurfaceMode, bootstrap_destination, bootstrap_source, encoded_destination_registration,
    encoded_source_registration, local_destination_registration, local_source_registration,
};
use weld_ssd::SsdPlugin;
use weld_window::WindowPlugin;
use weld_window_ui::WindowUiPlugin;

pub use arguments::{AppArguments, BackendKind};

pub fn run(arguments: AppArguments) -> Result<()> {
    telemetry::initialize()?;
    if (arguments.hoist_codec.is_some() || arguments.hoist_encoded_dump_dir.is_some())
        && arguments.hoist_surface_mode != Some(arguments::HoistSurfaceMode::EncodedOpaque)
    {
        anyhow::bail!("codec and encoded diagnostics require --hoist-surface-mode encoded-opaque");
    }
    let hoist_codec = arguments.hoist_codec.unwrap_or_default();

    enum PendingLocalTransport {
        Source(LocalPacketListener),
        Destination(std::path::PathBuf),
    }
    let pending_local_transport = if let Some(path) = &arguments.hoist_listen {
        Some(PendingLocalTransport::Source(LocalPacketListener::bind(
            path,
            LocalPeerRole::Source,
        )?))
    } else {
        arguments
            .hoist_connect
            .clone()
            .map(PendingLocalTransport::Destination)
    };

    let mut app = WeldApp::builder()
        .backend(arguments.backend.as_backend())
        .launch(arguments.client)
        .screenshot(arguments.screenshot)
        .remote_debug(arguments.remote_debug)
        .scale(arguments.scale)
        .socket_name(arguments.wayland_socket)
        .build()?;
    let local_transport = match pending_local_transport {
        Some(PendingLocalTransport::Source(listener)) => {
            let mode = arguments
                .hoist_surface_mode
                .map(|mode| mode.local(hoist_codec))
                .unwrap_or(LocalSurfaceMode::Native);
            if let LocalSurfaceMode::EncodedOpaque(codec) = mode {
                validate_encoded_media(&app, codec)?;
            }
            Some((
                LocalPeerRole::Source,
                bootstrap_source(listener.accept_blocking()?, mode)?,
            ))
        }
        Some(PendingLocalTransport::Destination(path)) => {
            let control = LocalPacketConnection::connect(path, LocalPeerRole::Destination)?;
            let capabilities = app.external_dmabuf_capabilities()?;
            Some((
                LocalPeerRole::Destination,
                bootstrap_destination(control, |mode| {
                    if let LocalSurfaceMode::EncodedOpaque(codec) = mode {
                        validate_encoded_capabilities(capabilities.as_ref(), codec)?;
                    }
                    Ok(())
                })?,
            ))
        }
        None => None,
    };
    let mut enable_hoist_policy = true;
    match local_transport {
        Some((LocalPeerRole::Source, transport)) => {
            let adapter_source = weld_client::ClientSourceId::new(1);
            match (transport.mode, transport.media) {
                (LocalSurfaceMode::Native, None) => {
                    let (adapter, endpoint) = local_source_registration(
                        transport.control.clone(),
                        weld_core::WAYLAND_CLIENT_SOURCE,
                        adapter_source,
                        adapter_source,
                    );
                    app.add_client_wake_source(transport.control.runtime_wake_source())
                        .add_client_adapter(adapter)
                        .insert_resource(HoistEndpointRegistry::with_default(endpoint));
                }
                (LocalSurfaceMode::EncodedOpaque(codec), Some(media)) => {
                    tracing::info!(?codec, "selected local hoist encoded codec");
                    let capabilities = required_external_capabilities(&app)?;
                    let (adapter, endpoint, wakes) = encoded_source_registration(
                        transport.control,
                        media,
                        EncodedSourceRegistrationOptions {
                            upstream_source: weld_core::WAYLAND_CLIENT_SOURCE,
                            adapter_source,
                            destination_source: adapter_source,
                            capabilities: &capabilities,
                            codec,
                            dump_directory: arguments.hoist_encoded_dump_dir,
                        },
                    )?;
                    for wake in wakes {
                        app.add_client_wake_source(wake);
                    }
                    app.add_client_adapter(adapter)
                        .insert_resource(HoistEndpointRegistry::with_default(endpoint));
                }
                _ => anyhow::bail!("local hoist bootstrap returned an invalid media channel"),
            }
        }
        Some((LocalPeerRole::Destination, transport)) => {
            let destination_source = weld_client::ClientSourceId::new(1);
            match (transport.mode, transport.media) {
                (LocalSurfaceMode::Native, None) => {
                    let adapter = local_destination_registration(
                        transport.control.clone(),
                        weld_core::WAYLAND_CLIENT_SOURCE,
                        destination_source,
                        app.dmabuf_context(),
                    );
                    app.add_client_wake_source(transport.control.runtime_wake_source())
                        .add_client_adapter(adapter);
                }
                (LocalSurfaceMode::EncodedOpaque(codec), Some(media)) => {
                    let capabilities = required_external_capabilities(&app)?;
                    let (adapter, wakes) = encoded_destination_registration(
                        transport.control,
                        media,
                        weld_core::WAYLAND_CLIENT_SOURCE,
                        destination_source,
                        app.dmabuf_context(),
                        &capabilities,
                        codec,
                    )?;
                    for wake in wakes {
                        app.add_client_wake_source(wake);
                    }
                    app.add_client_adapter(adapter);
                }
                _ => anyhow::bail!("local hoist bootstrap returned an invalid media channel"),
            }
            enable_hoist_policy = false;
        }
        None => {
            let (adapter, endpoint) = loopback_registration(
                weld_core::WAYLAND_CLIENT_SOURCE,
                weld_client::ClientSourceId::new(1),
            );
            app.add_client_adapter(adapter)
                .insert_resource(HoistEndpointRegistry::with_default(endpoint));
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

fn required_external_capabilities(
    app: &WeldApp,
) -> Result<weld_core::dmabuf::ExternalDmabufCapabilities> {
    app.external_dmabuf_capabilities()?
        .ok_or_else(|| anyhow::anyhow!("selected Weld GPU cannot import DMA-BUFs"))
}

fn validate_encoded_media(app: &WeldApp, codec: weld_media::VideoCodec) -> Result<()> {
    let capabilities = required_external_capabilities(app)?;
    validate_encoded_capabilities(Some(&capabilities), codec)
}

fn validate_encoded_capabilities(
    capabilities: Option<&weld_core::dmabuf::ExternalDmabufCapabilities>,
    codec: weld_media::VideoCodec,
) -> Result<()> {
    let capabilities =
        capabilities.context("selected Weld GPU cannot import DMA-BUFs for decoded video")?;
    let media = weld_media_vaapi::probe_vaapi_device(&capabilities.render_node)
        .map_err(anyhow::Error::new)?;
    anyhow::ensure!(
        media.supports_round_trip(codec),
        "{} exposes no complete hardware {:?} and VPP path",
        media.vendor,
        codec,
    );
    Ok(())
}

pub fn run_from_env() -> Result<()> {
    run(AppArguments::parse())
}
