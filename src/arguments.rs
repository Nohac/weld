//! Command-line configuration for the standard Weld distribution.

use std::{ffi::OsString, path::PathBuf};

use clap::{Parser, ValueEnum};
use weld_app::{Backend, OutputScale};
use weld_hoist_local::LocalSurfaceMode;
use weld_media::VideoCodec;

const DEFAULT_REMOTE_ADDRESS: &str = "127.0.0.1:15702";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum BackendKind {
    #[default]
    Auto,
    Nested,
    Drm,
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
    pub(crate) fn as_backend(self) -> Backend {
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

    /// Standalone DRM output scale. Fractional values are supported; clients
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

    /// Source-side surface carrier used for a local hoist peer.
    #[arg(long, value_enum, value_name = "MODE", requires = "hoist_listen")]
    pub(crate) hoist_surface_mode: Option<HoistSurfaceMode>,

    /// Codec used by the opaque encoded local-hoist carrier.
    #[arg(long, value_enum, value_name = "CODEC", requires = "hoist_listen")]
    pub(crate) hoist_codec: Option<HoistCodec>,

    /// Record source encoded stream generations before local transport.
    #[arg(long, value_name = "DIR", requires = "hoist_listen")]
    pub(crate) hoist_encoded_dump_dir: Option<PathBuf>,

    /// Optional client program followed by its arguments.
    #[arg(value_name = "CLIENT_AND_ARGS", allow_hyphen_values = true)]
    pub(crate) client: Vec<OsString>,
}
