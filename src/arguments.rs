//! Command-line configuration for the standard Weld distribution.

use std::{ffi::OsString, path::PathBuf};

use clap::{ArgGroup, Parser, ValueEnum};
use weld_app::{Backend, OutputScale};
use weld_hoist_local::LocalSurfaceMode;
use weld_media::VideoCodec;

const DEFAULT_REMOTE_ADDRESS: &str = "127.0.0.1:15702";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum LegacyKeyRepeatArgument {
    #[default]
    Client,
    Disabled,
    Emulated,
}

impl From<LegacyKeyRepeatArgument> for weld_app::input::LegacyKeyRepeat {
    fn from(value: LegacyKeyRepeatArgument) -> Self {
        match value {
            LegacyKeyRepeatArgument::Client => Self::Client,
            LegacyKeyRepeatArgument::Disabled => Self::Disabled,
            LegacyKeyRepeatArgument::Emulated => Self::Emulated,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum KeyboardRepeatModeArgument {
    Client,
    Compositor,
}

impl From<KeyboardRepeatModeArgument> for weld_app::input::KeyboardRepeatMode {
    fn from(value: KeyboardRepeatModeArgument) -> Self {
        match value {
            KeyboardRepeatModeArgument::Client => Self::Client,
            KeyboardRepeatModeArgument::Compositor => Self::Compositor,
        }
    }
}

pub(crate) fn resolve_legacy_repeat(
    argument: Option<LegacyKeyRepeatArgument>,
    environment: Option<&str>,
) -> Result<LegacyKeyRepeatArgument, String> {
    if let Some(argument) = argument {
        return Ok(argument);
    }
    environment.map_or(Ok(LegacyKeyRepeatArgument::default()), |value| {
        LegacyKeyRepeatArgument::from_str(value, false)
    })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum BackendKind {
    #[default]
    Auto,
    Nested,
    Drm,
    Headless,
}

/// The session-only host deliberately does not construct a Bevy application.
pub(crate) enum HostSelection {
    Bevy(Backend),
    Headless,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum HoistSurfaceMode {
    Native,
    EncodedOpaque,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum HoistCodec {
    Av1,
    #[default]
    H264,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum IrohNetworkKind {
    #[default]
    Direct,
    N0,
}

impl From<IrohNetworkKind> for weld_hoist_iroh::IrohNetwork {
    fn from(network: IrohNetworkKind) -> Self {
        match network {
            IrohNetworkKind::Direct => Self::Direct,
            IrohNetworkKind::N0 => Self::N0,
        }
    }
}

impl From<HoistCodec> for VideoCodec {
    fn from(codec: HoistCodec) -> Self {
        match codec {
            HoistCodec::Av1 => Self::Av1,
            HoistCodec::H264 => Self::H264,
        }
    }
}

impl HoistSurfaceMode {
    pub(crate) fn local(self, codec: HoistCodec) -> LocalSurfaceMode {
        match self {
            Self::Native => LocalSurfaceMode::Native,
            Self::EncodedOpaque => LocalSurfaceMode::EncodedOpaque(codec.into()),
        }
    }
}

impl BackendKind {
    pub(crate) fn selection(self) -> HostSelection {
        match self {
            Self::Auto => HostSelection::Bevy(Backend::Auto),
            Self::Nested => HostSelection::Bevy(Backend::Nested),
            Self::Drm => HostSelection::Bevy(Backend::Drm),
            Self::Headless => HostSelection::Headless,
        }
    }
}

#[derive(Parser)]
#[command(
    version,
    about = "Bevy-native Wayland compositor",
    trailing_var_arg = true,
    group(ArgGroup::new("hoist_peer").args([
        "hoist_listen",
        "hoist_connect",
        "hoist_iroh_listen",
        "hoist_iroh_connect",
    ]).multiple(false)),
    group(ArgGroup::new("hoist_iroh_peer").args([
        "hoist_iroh_listen",
        "hoist_iroh_connect",
    ]).multiple(false))
)]
pub struct AppArguments {
    /// Legacy repeat fallback: client timers, disabled, or emulated key edges. Defaults to client;
    /// WELD_LEGACY_KEY_REPEAT supplies a default when this option is absent.
    #[arg(long, value_enum)]
    pub(crate) legacy_key_repeat: Option<LegacyKeyRepeatArgument>,

    /// Stable repeat owner. Defaults to compositor for nested/headless Iroh, client otherwise.
    /// Use client if the parent compositor supplies no repeat cadence.
    #[arg(long, value_enum)]
    pub(crate) keyboard_repeat_mode: Option<KeyboardRepeatModeArgument>,
    /// Host backend. Auto uses a nested host when available and DRM on a TTY.
    #[arg(long, value_enum, default_value_t)]
    pub(crate) backend: BackendKind,

    /// Headless virtual output in physical pixels (default 1920x1080).
    #[arg(long, value_name = "WIDTHxHEIGHT", value_parser = parse_extent)]
    pub(crate) headless_output: Option<weld_core::surface::Extent>,

    /// Headless initial toplevel size in logical pixels (default 960x640).
    #[arg(long, value_name = "WIDTHxHEIGHT", value_parser = parse_extent)]
    pub(crate) headless_window_size: Option<weld_core::surface::Extent>,

    /// Virtual output refresh and policy cadence (default 60 Hz); no drawing without a presenter.
    #[arg(long, value_name = "HZ", value_parser = clap::value_parser!(u32).range(1..=240))]
    pub(crate) headless_refresh: Option<u32>,

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

    /// DRM or headless output scale. Fractional values are supported; clients
    /// without fractional-scale support receive the rounded Wayland scale.
    #[arg(long, value_name = "FACTOR")]
    pub(crate) scale: Option<OutputScale>,

    /// Wayland socket name exposed by this compositor instance.
    #[arg(long, value_name = "NAME")]
    pub(crate) wayland_socket: Option<String>,

    /// Accept one sibling Weld destination over a local Unix seqpacket socket.
    #[arg(long, value_name = "PATH", conflicts_with = "hoist_connect")]
    pub(crate) hoist_listen: Option<PathBuf>,

    /// Import hoisted windows from a sibling Weld source.
    #[arg(long, value_name = "PATH", conflicts_with = "hoist_listen")]
    pub(crate) hoist_connect: Option<PathBuf>,

    /// Publish an Iroh ticket and accept one encoded Weld destination.
    #[arg(long, value_name = "PATH", requires = "hoist_iroh_expect_peer")]
    pub(crate) hoist_iroh_listen: Option<PathBuf>,

    /// Consent to hoist every existing and future window in a new headless session.
    #[arg(long, requires = "hoist_iroh_listen")]
    pub(crate) hoist_all: bool,

    /// Import encoded windows from the Weld source named by an Iroh ticket.
    #[arg(long, value_name = "PATH", requires = "hoist_iroh_publish_identity")]
    pub(crate) hoist_iroh_connect: Option<PathBuf>,

    /// Trusted private file containing the only destination identity to admit.
    #[arg(long, value_name = "PATH", requires = "hoist_iroh_listen")]
    pub(crate) hoist_iroh_expect_peer: Option<PathBuf>,

    /// Publish this destination's public identity for explicit source approval.
    #[arg(long, value_name = "PATH", requires = "hoist_iroh_connect")]
    pub(crate) hoist_iroh_publish_identity: Option<PathBuf>,

    /// Retain this device's private Iroh key across runs (otherwise ephemeral).
    #[arg(long, value_name = "DIRECTORY", requires = "hoist_iroh_peer")]
    pub(crate) hoist_iroh_device_dir: Option<PathBuf>,

    /// Publish a new private source profile. N0 supports rediscovery after restart;
    /// Direct address hints must be refreshed after rebinding.
    #[arg(long, value_name = "PATH", requires = "hoist_iroh_listen")]
    pub(crate) hoist_iroh_publish_profile: Option<PathBuf>,

    /// Rendezvous and peer startup budget in seconds (default 120, maximum 3600).
    #[arg(long, value_name = "SECONDS", requires = "hoist_iroh_peer", value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub(crate) hoist_iroh_timeout: Option<u64>,

    /// Iroh connectivity preset. Direct has no DNS or relay dependency.
    #[arg(long, value_enum, value_name = "PRESET", requires = "hoist_iroh_peer")]
    pub(crate) hoist_iroh_network: Option<IrohNetworkKind>,

    /// Source-side surface carrier used for a local hoist peer.
    #[arg(long, value_enum, value_name = "MODE", requires = "hoist_listen")]
    pub(crate) hoist_surface_mode: Option<HoistSurfaceMode>,

    /// Codec used by an opaque encoded hoist source.
    #[arg(long, value_enum, value_name = "CODEC")]
    pub(crate) hoist_codec: Option<HoistCodec>,

    /// Shared encoder target, not a bandwidth cap (AV1: 8 Mbps; H.264: 16 Mbps).
    #[arg(long, value_name = "MBPS", value_parser = clap::value_parser!(u64).range(1..=u64::MAX / 1_000_000))]
    pub(crate) hoist_bitrate_target_mbps: Option<u64>,

    /// Record source encoded stream generations before transport.
    #[arg(long, value_name = "DIR")]
    pub(crate) hoist_encoded_dump_dir: Option<PathBuf>,

    /// Optional client program followed by its arguments.
    #[arg(value_name = "CLIENT_AND_ARGS", allow_hyphen_values = true)]
    pub(crate) client: Vec<OsString>,
}

fn parse_extent(value: &str) -> Result<weld_core::surface::Extent, String> {
    let (width, height) = value.split_once('x').ok_or("expected WIDTHxHEIGHT")?;
    let width = width.parse::<u32>().map_err(|_| "invalid width")?;
    let height = height.parse::<u32>().map_err(|_| "invalid height")?;
    if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
        return Err("dimensions must be between 1 and 8192".to_owned());
    }
    Ok(weld_core::surface::Extent::new(width, height))
}
