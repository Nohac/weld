//! Standard Weld compositor distribution and backend selection.

mod arguments;
mod bitrate_budget;
mod headless;
mod iroh_host;
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
use weld_core::{runtime::RuntimeOptions, surface::Extent};
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
    validate_session_arguments(&arguments)?;
    let backend = match arguments.backend.selection() {
        arguments::HostSelection::Bevy(backend) => backend,
        arguments::HostSelection::Headless => {
            // Validation and construction are one path: no transport can bind
            // and no Bevy renderer can start before the session-only branch.
            return headless::run(arguments);
        }
    };
    validate_hoist_arguments(&arguments)?;
    let hoist_codec = arguments.hoist_codec.unwrap_or_default();
    let bitrate_budget = bitrate_budget::for_source(&arguments)?;
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
            host: iroh_host::bind(&arguments)?,
            ticket: ticket.clone(),
        })
    } else if let Some(ticket) = &arguments.hoist_iroh_connect {
        let host = iroh_host::bind(&arguments)?;
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
        .backend(backend)
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
                            bitrate_budget,
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
                    validate_encoded_capabilities(
                        capabilities.as_ref(),
                        codec,
                        MediaOperation::Decode,
                    )?;
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
                network_notifier.into(),
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
                adapter_source,
                IrohSourceRegistrationOptions {
                    upstream_source: weld_core::WAYLAND_CLIENT_SOURCE,
                    adapter_source,
                    capabilities: &capabilities,
                    codec,
                    dump_directory: arguments.hoist_encoded_dump_dir,
                    bitrate_budget,
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
            let peer = host.connect_destination(
                ticket,
                supported_codecs,
                network_notifier.into(),
                iroh_timeout,
            )?;
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

fn validate_session_arguments(arguments: &AppArguments) -> Result<()> {
    if arguments.backend != BackendKind::Headless {
        anyhow::ensure!(
            !arguments.hoist_all,
            "--hoist-all requires --backend headless"
        );
        anyhow::ensure!(
            arguments.headless_output.is_none()
                && arguments.headless_window_size.is_none()
                && arguments.headless_refresh.is_none(),
            "headless output/window/refresh options require --backend headless"
        );
        return Ok(());
    }
    anyhow::ensure!(
        arguments.screenshot.is_none() && arguments.remote_debug.is_none(),
        "headless sessions do not provide screenshots or Bevy remote debugging"
    );
    anyhow::ensure!(
        arguments.hoist_listen.is_none()
            && arguments.hoist_connect.is_none()
            && arguments.hoist_iroh_connect.is_none(),
        "headless sessions support only an Iroh source transport"
    );
    anyhow::ensure!(
        arguments.hoist_iroh_listen.is_some() == arguments.hoist_all,
        "headless Iroh hosting requires explicit --hoist-all consent"
    );
    validate_hoist_arguments(arguments)?;
    Ok(())
}

fn runtime_options(arguments: &AppArguments) -> Result<RuntimeOptions> {
    Ok(RuntimeOptions::new(
        arguments.headless_output.unwrap_or(Extent::new(1920, 1080)),
        arguments.scale.unwrap_or_default(),
        arguments.headless_refresh.unwrap_or(60),
        arguments
            .headless_window_size
            .unwrap_or(Extent::new(960, 640)),
    )?
    .socket_name(arguments.wayland_socket.clone())
    .launch(arguments.client.clone())
    .keyboard_repeat(
        headless_repeat_mode(arguments),
        arguments
            .legacy_key_repeat
            .map(Into::into)
            .unwrap_or_default(),
    ))
}

fn headless_repeat_mode(arguments: &AppArguments) -> weld_core::input::KeyboardRepeatMode {
    arguments.keyboard_repeat_mode.map(Into::into).unwrap_or(
        if arguments.hoist_iroh_listen.is_some() {
            weld_core::input::KeyboardRepeatMode::Compositor
        } else {
            weld_core::input::KeyboardRepeatMode::Client
        },
    )
}

fn validate_hoist_arguments(arguments: &AppArguments) -> Result<()> {
    let encoded_source = arguments.hoist_iroh_listen.is_some()
        || (arguments.hoist_listen.is_some()
            && arguments.hoist_surface_mode == Some(arguments::HoistSurfaceMode::EncodedOpaque));
    if (arguments.hoist_codec.is_some()
        || arguments.hoist_encoded_dump_dir.is_some()
        || arguments.hoist_bitrate_target_mbps.is_some())
        && !encoded_source
    {
        anyhow::bail!(
            "codec, bitrate target and encoded diagnostics require an encoded hoist source"
        );
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
    validate_encoded_capabilities(Some(&capabilities), codec, MediaOperation::Encode)
}

#[derive(Clone, Copy, Debug)]
enum MediaOperation {
    Encode,
    Decode,
}

fn media_supported(
    media: &weld_media_vaapi::VaapiCapabilities,
    codec: weld_media::VideoCodec,
    operation: MediaOperation,
) -> bool {
    match operation {
        MediaOperation::Encode => media.supports_encode(codec),
        MediaOperation::Decode => media.supports_decode(codec),
    }
}

fn validate_encoded_capabilities(
    capabilities: Option<&weld_core::dmabuf::ExternalDmabufCapabilities>,
    codec: weld_media::VideoCodec,
    operation: MediaOperation,
) -> Result<()> {
    let capabilities =
        capabilities.context("selected Weld GPU cannot import DMA-BUFs for encoded hoisting")?;
    let media = weld_media_vaapi::probe_vaapi_device(&capabilities.render_node)
        .map_err(anyhow::Error::new)?;
    anyhow::ensure!(
        media_supported(&media, codec, operation),
        "{} exposes no hardware {:?} {:?} and VPP path",
        media.vendor,
        operation,
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
    fn headless_cli_selects_a_session_without_changing_auto() {
        assert!(matches!(
            BackendKind::Auto.selection(),
            arguments::HostSelection::Bevy(weld_app::Backend::Auto)
        ));
        assert!(matches!(
            BackendKind::Headless.selection(),
            arguments::HostSelection::Headless
        ));
        let args = arguments(&[
            "--backend",
            "headless",
            "--headless-output",
            "2560x1440",
            "--headless-window-size",
            "1280x720",
            "--headless-refresh",
            "90",
            "--scale",
            "1.5",
        ]);
        validate_session_arguments(&args).expect("headless arguments");
        runtime_options(&args).expect("headless configuration");
    }

    #[test]
    fn headless_iroh_requires_whole_session_consent_and_owns_repeat_by_default() {
        let flags = [
            "--backend",
            "headless",
            "--hoist-iroh-listen",
            "ticket",
            "--hoist-iroh-expect-peer",
            "peer",
            "--hoist-all",
        ];
        let args = arguments(&flags);
        validate_session_arguments(&args).expect("explicit session consent");
        runtime_options(&args).expect("runtime options");
        assert_eq!(
            headless_repeat_mode(&args),
            weld_core::input::KeyboardRepeatMode::Compositor
        );
        let explicit =
            arguments(&[flags.as_slice(), &["--keyboard-repeat-mode", "client"]].concat());
        assert_eq!(
            headless_repeat_mode(&explicit),
            weld_core::input::KeyboardRepeatMode::Client
        );
        let mut nested = args;
        nested.backend = BackendKind::Nested;
        assert!(validate_session_arguments(&nested).is_err());
    }

    #[test]
    fn encoded_capability_gates_are_role_specific() {
        let mut media = weld_media_vaapi::VaapiCapabilities {
            vendor: "test".to_owned(),
            h264_decode: false,
            av1_decode: false,
            h264_encode: Some(weld_media_vaapi::VaapiEncodeEntrypoint::Slice),
            av1_encode: None,
            video_processing: true,
        };
        assert!(media_supported(
            &media,
            weld_media::VideoCodec::H264,
            MediaOperation::Encode
        ));
        assert!(!media_supported(
            &media,
            weld_media::VideoCodec::H264,
            MediaOperation::Decode
        ));
        media.h264_encode = None;
        media.h264_decode = true;
        assert!(media_supported(
            &media,
            weld_media::VideoCodec::H264,
            MediaOperation::Decode
        ));
        assert!(!media_supported(
            &media,
            weld_media::VideoCodec::H264,
            MediaOperation::Encode
        ));
    }

    #[test]
    fn headless_cli_rejects_unsupported_presenters_and_transports_before_startup() {
        for flags in [
            vec!["--headless-refresh", "90"],
            vec!["--backend", "nested", "--headless-output", "1920x1080"],
            vec!["--backend", "headless", "--screenshot", "unused.png"],
            vec!["--backend", "headless", "--remote-debug"],
            vec!["--backend", "headless", "--hoist-listen", "unused.sock"],
            vec![
                "--backend",
                "headless",
                "--hoist-iroh-listen",
                "ticket",
                "--hoist-iroh-expect-peer",
                "peer",
            ],
        ] {
            assert!(
                validate_session_arguments(&arguments(&flags)).is_err(),
                "{flags:?}"
            );
        }
        for flags in [
            vec!["--headless-output", "0x1080"],
            vec!["--headless-window-size", "9000x10"],
            vec!["--headless-output", "1920x1080x1"],
            vec!["--headless-refresh", "0"],
            vec!["--headless-refresh", "241"],
        ] {
            assert!(AppArguments::try_parse_from(std::iter::once("weldwm").chain(flags)).is_err());
        }
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
        assert_eq!(
            resolve_legacy_repeat(None, Some("emulated")),
            Ok(LegacyKeyRepeatArgument::Emulated)
        );
        let emulated = arguments(&["--legacy-key-repeat", "emulated"]);
        assert_eq!(
            resolve_legacy_repeat(emulated.legacy_key_repeat, Some("disabled")),
            Ok(LegacyKeyRepeatArgument::Emulated)
        );
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
    fn persistent_identity_and_profile_require_explicit_iroh_roles() {
        for flag in ["--hoist-iroh-device-dir", "--hoist-iroh-publish-profile"] {
            assert!(AppArguments::try_parse_from(["weldwm", flag, "/tmp/example"]).is_err());
        }
        for backend in ["nested", "headless"] {
            let parsed = AppArguments::try_parse_from([
                "weldwm",
                "--backend",
                backend,
                "--hoist-iroh-listen",
                "/tmp/ticket",
                "--hoist-iroh-expect-peer",
                "/tmp/approved",
                "--hoist-iroh-device-dir",
                "/tmp/device",
                "--hoist-iroh-publish-profile",
                "/tmp/profile",
                "--hoist-iroh-network",
                "n0",
            ])
            .expect("explicit persistent source");
            assert_eq!(
                parsed.hoist_iroh_device_dir.as_deref(),
                Some(std::path::Path::new("/tmp/device"))
            );
            assert_eq!(
                parsed.hoist_iroh_publish_profile.as_deref(),
                Some(std::path::Path::new("/tmp/profile"))
            );
        }
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
