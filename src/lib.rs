//! Standard Weld compositor distribution and backend selection.

mod arguments;
mod overlay;
mod telemetry;

use anyhow::{Context, Result};
use clap::Parser;
use overlay::DistributionOverlayPlugin;
use std::time::Duration;
use weld_app::{
    WeldApp,
    input::{GlobalShortcutPlugin, VirtualTerminalShortcutPlugin},
};
use weld_float::FloatPlugin;
use weld_hoist::{HoistEndpointRegistry, HoistPlugin, loopback_registration};
use weld_hoist_iroh::{
    IrohHost, IrohSourceRegistrationOptions,
    destination_registration as iroh_destination_registration,
    source_registration as iroh_source_registration,
};
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
    validate_hoist_arguments(&arguments)?;
    let hoist_codec = arguments.hoist_codec.unwrap_or_default();
    let iroh_timeout = Duration::from_secs(arguments.hoist_iroh_timeout.unwrap_or(120));

    enum PendingHoistTransport {
        LocalSource(LocalPacketListener),
        LocalDestination(std::path::PathBuf),
        IrohSource {
            host: IrohHost,
            ticket: std::path::PathBuf,
        },
        IrohDestination {
            host: IrohHost,
            ticket: std::path::PathBuf,
        },
    }
    let pending_transport = if let Some(path) = &arguments.hoist_listen {
        Some(PendingHoistTransport::LocalSource(
            LocalPacketListener::bind(path, LocalPeerRole::Source)?,
        ))
    } else if let Some(path) = &arguments.hoist_connect {
        Some(PendingHoistTransport::LocalDestination(path.clone()))
    } else if let Some(ticket) = &arguments.hoist_iroh_listen {
        Some(PendingHoistTransport::IrohSource {
            host: IrohHost::bind(arguments.hoist_iroh_network.unwrap_or_default().into())?,
            ticket: ticket.clone(),
        })
    } else if let Some(ticket) = &arguments.hoist_iroh_connect {
        let host = IrohHost::bind(arguments.hoist_iroh_network.unwrap_or_default().into())?;
        host.publish_identity(
            arguments
                .hoist_iroh_publish_identity
                .as_ref()
                .context("Iroh destination needs an identity publication path")?,
        )?;
        Some(PendingHoistTransport::IrohDestination {
            host,
            ticket: ticket.clone(),
        })
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
        .keyboard_repeat_mode(arguments.keyboard_repeat_mode.map(Into::into))
        .build()?;
    app.insert_resource(weld_app::input::KeyboardSettings {
        legacy_repeat: arguments
            .legacy_key_repeat
            .map(Into::into)
            .unwrap_or_default(),
    });
    let mut enable_hoist_policy = true;
    match pending_transport {
        Some(PendingHoistTransport::LocalSource(listener)) => {
            let mode = arguments
                .hoist_surface_mode
                .map(|mode| mode.local(hoist_codec))
                .unwrap_or(LocalSurfaceMode::Native);
            if let LocalSurfaceMode::EncodedOpaque(codec) = mode {
                validate_encoded_media(&app, codec)?;
            }
            let transport = bootstrap_source(listener.accept_blocking()?, mode)?;
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
        Some(PendingHoistTransport::LocalDestination(path)) => {
            let control = LocalPacketConnection::connect(path, LocalPeerRole::Destination)?;
            let capabilities = app.external_dmabuf_capabilities()?;
            let transport = bootstrap_destination(control, |mode| {
                if let LocalSurfaceMode::EncodedOpaque(codec) = mode {
                    validate_encoded_capabilities(capabilities.as_ref(), codec)?;
                }
                Ok(())
            })?;
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
        Some(PendingHoistTransport::IrohSource { host, ticket }) => {
            let codec: weld_media::VideoCodec = hoist_codec.into();
            validate_encoded_media(&app, codec)?;
            let (network_notifier, network_wake) = weld_core::host::client_runtime_notifier()?;
            let peer = host.accept_source(
                ticket,
                arguments
                    .hoist_iroh_expect_peer
                    .as_ref()
                    .context("Iroh source needs an approved peer identity path")?,
                codec,
                network_notifier,
                iroh_timeout,
            )?;
            tracing::info!(
                peer = peer.identity().as_str(),
                ?codec,
                "connected Iroh hoist destination"
            );
            let capabilities = required_external_capabilities(&app)?;
            let adapter_source = weld_client::ClientSourceId::new(1);
            let (adapter, endpoint, codec_wake) = iroh_source_registration(
                peer,
                IrohSourceRegistrationOptions {
                    upstream_source: weld_core::WAYLAND_CLIENT_SOURCE,
                    adapter_source,
                    destination_source: adapter_source,
                    capabilities: &capabilities,
                    codec,
                    dump_directory: arguments.hoist_encoded_dump_dir,
                },
            )?;
            app.add_client_wake_source(network_wake)
                .add_client_wake_source(codec_wake)
                .add_client_adapter(adapter)
                .insert_resource(HoistEndpointRegistry::with_default(endpoint));
        }
        Some(PendingHoistTransport::IrohDestination { host, ticket }) => {
            let capabilities = required_external_capabilities(&app)?;
            let media = weld_media_vaapi::probe_vaapi_device(&capabilities.render_node)
                .map_err(anyhow::Error::new)?;
            let supported_codecs = [weld_media::VideoCodec::Av1, weld_media::VideoCodec::H264]
                .into_iter()
                .filter(|codec| media.supports_decode(*codec))
                .collect::<Vec<_>>();
            anyhow::ensure!(
                !supported_codecs.is_empty(),
                "{} exposes no supported hardware decoder and VPP path",
                media.vendor
            );
            let (network_notifier, network_wake) = weld_core::host::client_runtime_notifier()?;
            let peer =
                host.connect_destination(ticket, supported_codecs, network_notifier, iroh_timeout)?;
            let codec = peer.codec();
            tracing::info!(
                peer = peer.identity().as_str(),
                ?codec,
                "connected to Iroh hoist source"
            );
            let destination_source = weld_client::ClientSourceId::new(1);
            let (adapter, codec_wake) = iroh_destination_registration(
                peer,
                weld_core::WAYLAND_CLIENT_SOURCE,
                destination_source,
                app.dmabuf_context(),
                &capabilities,
                codec,
            )?;
            app.add_client_wake_source(network_wake)
                .add_client_wake_source(codec_wake)
                .add_client_adapter(adapter);
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

fn validate_hoist_arguments(arguments: &AppArguments) -> Result<()> {
    let encoded_source = arguments.hoist_iroh_listen.is_some()
        || (arguments.hoist_listen.is_some()
            && arguments.hoist_surface_mode == Some(arguments::HoistSurfaceMode::EncodedOpaque));
    if (arguments.hoist_codec.is_some() || arguments.hoist_encoded_dump_dir.is_some())
        && !encoded_source
    {
        anyhow::bail!("codec and encoded diagnostics require an encoded hoist source");
    }
    Ok(())
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
    let mut arguments = AppArguments::parse();
    if arguments.legacy_key_repeat.is_none() {
        let environment = match std::env::var("WELD_LEGACY_KEY_REPEAT") {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(error) => return Err(error).context("invalid WELD_LEGACY_KEY_REPEAT"),
        };
        arguments.legacy_key_repeat = Some(
            arguments::resolve_legacy_repeat(None, environment.as_deref())
                .map_err(anyhow::Error::msg)
                .context("invalid WELD_LEGACY_KEY_REPEAT")?,
        );
    }
    run(arguments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> AppArguments {
        AppArguments::try_parse_from(std::iter::once("weldwm").chain(values.iter().copied()))
            .expect("valid command line")
    }

    #[test]
    fn keyboard_repeat_configuration_is_explicit_and_cli_wins() {
        use crate::arguments::{LegacyKeyRepeatArgument, resolve_legacy_repeat};
        assert_eq!(
            resolve_legacy_repeat(None, None),
            Ok(LegacyKeyRepeatArgument::Client)
        );
        assert_eq!(
            resolve_legacy_repeat(None, Some("disabled")),
            Ok(LegacyKeyRepeatArgument::Disabled)
        );
        assert!(resolve_legacy_repeat(None, Some("typo")).is_err());
        let options = arguments(&[
            "--legacy-key-repeat",
            "client",
            "--keyboard-repeat-mode",
            "client",
        ]);
        assert_eq!(
            resolve_legacy_repeat(options.legacy_key_repeat, Some("disabled")),
            Ok(LegacyKeyRepeatArgument::Client)
        );
        assert!(options.keyboard_repeat_mode.is_some());
    }

    #[test]
    fn encoded_options_accept_both_source_transports_without_starting_a_host() {
        let local = arguments(&[
            "--hoist-listen",
            "/tmp/weld.sock",
            "--hoist-surface-mode",
            "encoded-opaque",
            "--hoist-codec",
            "av1",
        ]);
        let iroh = arguments(&[
            "--hoist-iroh-listen",
            "/tmp/weld.ticket",
            "--hoist-iroh-expect-peer",
            "/tmp/destination.identity",
            "--hoist-codec",
            "av1",
        ]);

        assert!(validate_hoist_arguments(&local).is_ok());
        assert!(validate_hoist_arguments(&iroh).is_ok());
    }

    #[test]
    fn encoded_options_reject_destinations_and_native_sources() {
        for arguments in [
            arguments(&["--hoist-connect", "/tmp/weld.sock", "--hoist-codec", "av1"]),
            arguments(&[
                "--hoist-iroh-connect",
                "/tmp/weld.ticket",
                "--hoist-iroh-publish-identity",
                "/tmp/destination.identity",
                "--hoist-codec",
                "av1",
            ]),
            arguments(&["--hoist-listen", "/tmp/weld.sock", "--hoist-codec", "av1"]),
        ] {
            assert!(validate_hoist_arguments(&arguments).is_err());
        }
    }

    #[test]
    fn peer_transports_are_mutually_exclusive() {
        assert!(
            AppArguments::try_parse_from([
                "weldwm",
                "--hoist-listen",
                "/tmp/weld.sock",
                "--hoist-iroh-connect",
                "/tmp/weld.ticket",
            ])
            .is_err()
        );
    }

    #[test]
    fn iroh_network_requires_an_iroh_peer() {
        assert!(
            AppArguments::try_parse_from(["weldwm", "--hoist-iroh-network", "direct"]).is_err()
        );
    }

    #[test]
    fn iroh_cli_requires_explicit_peer_exchange_and_bounded_timeout() {
        for args in [
            vec!["--hoist-iroh-listen", "/tmp/source"],
            vec!["--hoist-iroh-connect", "/tmp/source"],
            vec!["--hoist-iroh-expect-peer", "/tmp/peer"],
            vec!["--hoist-iroh-publish-identity", "/tmp/peer"],
            vec!["--hoist-iroh-timeout", "120"],
            vec![
                "--hoist-iroh-listen",
                "/tmp/source",
                "--hoist-iroh-expect-peer",
                "/tmp/peer",
                "--hoist-iroh-timeout",
                "0",
            ],
            vec![
                "--hoist-iroh-listen",
                "/tmp/source",
                "--hoist-iroh-expect-peer",
                "/tmp/peer",
                "--hoist-iroh-timeout",
                "3601",
            ],
        ] {
            assert!(AppArguments::try_parse_from(std::iter::once("weldwm").chain(args)).is_err());
        }
        for network in ["direct", "n0"] {
            let args = arguments(&[
                "--hoist-iroh-listen",
                "/tmp/source",
                "--hoist-iroh-expect-peer",
                "/tmp/peer",
                "--hoist-iroh-network",
                network,
                "--hoist-iroh-timeout",
                "300",
            ]);
            assert_eq!(args.hoist_iroh_timeout, Some(300));
        }
    }
}
