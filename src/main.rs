//! Weld compositor entry point and backend selection.

mod backend;
mod composition;
mod debug;
mod input;
mod layer;
mod renderer;
mod runtime;
mod server;
mod shell;
mod surface;
mod window;

use std::{
    env,
    ffi::{OsStr, OsString},
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow, bail};
#[cfg(debug_assertions)]
use bevy_dylib as _;
use calloop::signals::{Signal, Signals};
use clap::{Parser, ValueEnum};

const DEFAULT_REMOTE_ADDRESS: &str = "127.0.0.1:15702";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum BackendKind {
    #[default]
    Auto,
    Nested,
    Drm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResolvedBackend {
    Nested,
    Drm,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct BackendEnvironment {
    bare_vt_console: bool,
    wayland_host: bool,
    x11_host: bool,
    graphical_session: bool,
    tty_session: bool,
    virtual_terminal: bool,
}

impl BackendEnvironment {
    fn detect() -> Self {
        let session_type = env::var_os("XDG_SESSION_TYPE");
        Self {
            // The kernel console's TERM is narrower evidence than the
            // session variables, which may have been imported from tty1.
            bare_vt_console: environment_equals("TERM", "linux"),
            wayland_host: inherited_wayland_socket_available() || wayland_display_available(),
            // A stale DISPLAY merely makes nested startup fail safely. Avoid
            // probing or connecting to the X server during backend selection.
            x11_host: environment_nonempty("DISPLAY"),
            graphical_session: session_type
                .as_deref()
                .is_some_and(|value| value == "wayland" || value == "x11"),
            tty_session: session_type.as_deref() == Some(OsStr::new("tty")),
            virtual_terminal: env::var("XDG_VTNR")
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .is_some_and(|number| number > 0),
        }
    }
}

impl BackendKind {
    fn resolve(self, environment: BackendEnvironment) -> ResolvedBackend {
        match self {
            Self::Nested => ResolvedBackend::Nested,
            Self::Drm => ResolvedBackend::Drm,
            Self::Auto => resolve_auto_backend(environment),
        }
    }
}

fn resolve_auto_backend(environment: BackendEnvironment) -> ResolvedBackend {
    if environment.bare_vt_console {
        return ResolvedBackend::Drm;
    }
    if environment.wayland_host || environment.x11_host || environment.graphical_session {
        return ResolvedBackend::Nested;
    }
    if environment.tty_session || environment.virtual_terminal {
        return ResolvedBackend::Drm;
    }
    ResolvedBackend::Nested
}

fn inherited_wayland_socket_available() -> bool {
    env::var("WAYLAND_SOCKET")
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .is_some_and(|file_descriptor| file_descriptor >= 0)
}

fn wayland_display_available() -> bool {
    let Some(display) = env::var_os("WAYLAND_DISPLAY").filter(|value| !value.is_empty()) else {
        return false;
    };
    let display = Path::new(&display);
    let socket = if display.is_absolute() {
        display.to_path_buf()
    } else {
        let Some(runtime_directory) = env::var_os("XDG_RUNTIME_DIR") else {
            return false;
        };
        PathBuf::from(runtime_directory).join(display)
    };
    socket
        .metadata()
        .is_ok_and(|metadata| metadata.file_type().is_socket())
}

fn environment_nonempty(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn environment_equals(name: &str, expected: &str) -> bool {
    env::var_os(name).as_deref() == Some(OsStr::new(expected))
}

#[derive(Parser)]
#[command(
    version,
    about = "Bevy-native Wayland compositor",
    trailing_var_arg = true
)]
pub(crate) struct AppArguments {
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

    /// Optional client program followed by its arguments.
    #[arg(value_name = "CLIENT_AND_ARGS", allow_hyphen_values = true)]
    pub(crate) client: Vec<OsString>,
}

fn main() -> Result<()> {
    let arguments = AppArguments::parse();
    if arguments
        .screenshot
        .as_ref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
        bail!("--screenshot requires a non-empty path");
    }

    // calloop's signalfd mask must exist before Bevy or wgpu spawn worker
    // threads, because only subsequently created threads inherit this mask.
    let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM])
        .map_err(|error| anyhow!("failed to initialize process signal handling: {error}"))?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init()
        .map_err(|error| anyhow!("failed to initialize tracing: {error}"))?;

    let requested_backend = arguments.backend;
    let resolved_backend = requested_backend.resolve(BackendEnvironment::detect());
    tracing::info!(
        ?requested_backend,
        ?resolved_backend,
        "selected Weld backend"
    );
    match resolved_backend {
        ResolvedBackend::Nested => backend::nested::run(arguments, signals),
        ResolvedBackend::Drm => backend::drm::run(arguments, signals),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_backend_selection_prefers_concrete_hosts_and_bare_consoles() {
        let live_wayland_on_tty = BackendEnvironment {
            wayland_host: true,
            tty_session: true,
            virtual_terminal: true,
            ..Default::default()
        };
        assert_eq!(
            resolve_auto_backend(live_wayland_on_tty),
            ResolvedBackend::Nested
        );

        let x11_host = BackendEnvironment {
            x11_host: true,
            ..Default::default()
        };
        assert_eq!(resolve_auto_backend(x11_host), ResolvedBackend::Nested);

        let both_hosts = BackendEnvironment {
            wayland_host: true,
            x11_host: true,
            ..Default::default()
        };
        assert_eq!(resolve_auto_backend(both_hosts), ResolvedBackend::Nested);

        let clean_tty = BackendEnvironment {
            tty_session: true,
            virtual_terminal: true,
            ..Default::default()
        };
        assert_eq!(resolve_auto_backend(clean_tty), ResolvedBackend::Drm);

        let leaked_host_on_bare_console = BackendEnvironment {
            bare_vt_console: true,
            wayland_host: true,
            ..Default::default()
        };
        assert_eq!(
            resolve_auto_backend(leaked_host_on_bare_console),
            ResolvedBackend::Drm
        );

        assert_eq!(
            resolve_auto_backend(BackendEnvironment::default()),
            ResolvedBackend::Nested
        );
    }
}
