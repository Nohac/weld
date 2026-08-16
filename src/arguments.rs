//! Command-line configuration for the standard Weld distribution.

use std::{ffi::OsString, path::PathBuf};

use clap::{Parser, ValueEnum};
use weld_app::{Backend, OutputScale};

const DEFAULT_REMOTE_ADDRESS: &str = "127.0.0.1:15702";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum BackendKind {
    #[default]
    Auto,
    Nested,
    Drm,
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

    /// Optional client program followed by its arguments.
    #[arg(value_name = "CLIENT_AND_ARGS", allow_hyphen_values = true)]
    pub(crate) client: Vec<OsString>,
}
