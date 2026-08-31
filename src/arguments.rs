//! Command-line configuration for the standard Weld distribution.

use std::{ffi::OsString, path::PathBuf};

use clap::{Parser, ValueEnum};
use weld_app::{Backend, OutputScale};
use weld_hoist_local::{LocalH264Profile, LocalSurfaceMode};

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
    EncodedH264Opaque,
}

impl From<HoistSurfaceMode> for LocalSurfaceMode {
    fn from(mode: HoistSurfaceMode) -> Self {
        match mode {
            HoistSurfaceMode::Native => Self::Native,
            HoistSurfaceMode::EncodedH264Opaque => Self::EncodedH264Opaque,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum HoistH264Profile {
    #[default]
    Standard,
    HighBitrate,
    AllIdr,
}

impl From<HoistH264Profile> for LocalH264Profile {
    fn from(profile: HoistH264Profile) -> Self {
        match profile {
            HoistH264Profile::Standard => Self::Standard,
            HoistH264Profile::HighBitrate => Self::HighBitrate,
            HoistH264Profile::AllIdr => Self::AllIdr,
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

    /// Source-side H.264 diagnostic encoder profile.
    #[arg(long, value_enum, value_name = "PROFILE", requires = "hoist_listen")]
    pub(crate) hoist_h264_profile: Option<HoistH264Profile>,

    /// Record source H.264 stream generations before local transport.
    #[arg(long, value_name = "DIR", requires = "hoist_listen")]
    pub(crate) hoist_h264_dump_dir: Option<PathBuf>,

    /// Optional client program followed by its arguments.
    #[arg(value_name = "CLIENT_AND_ARGS", allow_hyphen_values = true)]
    pub(crate) client: Vec<OsString>,
}
