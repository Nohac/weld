//! Bootstrap API for a Bevy application hosted by Weld.

use std::{
    env,
    ffi::{OsStr, OsString},
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use bevy::{
    app::{App, Plugins},
    ecs::{
        resource::Resource,
        schedule::{IntoScheduleConfigs, ScheduleLabel},
        system::ScheduleSystem,
    },
};
use tracing::info;
use weld_core::{HostBackend, HostBuilder, PreparedHost};

use crate::{
    debug::{DebugProtocolPlugin, configure_remote_debug},
    shell::{AppShell, WeldAppPlugin, configure_rendering},
};

/// Requested native backend. [`Self::Auto`] resolves during [`WeldAppBuilder::build`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Backend {
    #[default]
    Auto,
    Nested,
    Drm,
}

/// Native backend actually prepared for this application.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Resource)]
pub enum ActiveBackend {
    Nested,
    Drm,
}

impl From<ActiveBackend> for HostBackend {
    fn from(backend: ActiveBackend) -> Self {
        match backend {
            ActiveBackend::Nested => Self::Nested,
            ActiveBackend::Drm => Self::Drm,
        }
    }
}

/// Reads Weld bootstrap state from an ordinary Bevy [`App`].
pub trait WeldAppExt {
    /// Returns the resolved backend, or `None` for an app not hosted by Weld.
    fn backend(&self) -> Option<ActiveBackend>;
}

impl WeldAppExt for App {
    fn backend(&self) -> Option<ActiveBackend> {
        self.world().get_resource::<ActiveBackend>().copied()
    }
}

/// Configures the immutable native roots of a [`WeldApp`].
#[derive(Default)]
pub struct WeldAppBuilder {
    backend: Backend,
    client: Vec<OsString>,
    screenshot: Option<PathBuf>,
    remote_debug: Option<String>,
}

impl WeldAppBuilder {
    /// Selects how Weld chooses its native host backend.
    pub fn backend(mut self, backend: Backend) -> Self {
        self.backend = backend;
        self
    }

    /// Configures an optional client command to launch when the host is ready.
    pub fn launch<I, S>(mut self, command: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.client = command.into_iter().map(Into::into).collect();
        self
    }

    /// Requests a startup screenshot at the given path.
    pub fn screenshot(mut self, path: Option<PathBuf>) -> Self {
        self.screenshot = path;
        self
    }

    /// Enables remote debugging at the given address.
    pub fn remote_debug(mut self, address: Option<String>) -> Self {
        self.remote_debug = address;
        self
    }

    /// Open the selected native host and create its configurable Bevy application.
    ///
    /// An exceptional nested-host exit during the initial blocking window pump
    /// is reported as an error because no application can be returned.
    pub fn build(self) -> Result<WeldApp> {
        if self
            .screenshot
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            bail!("screenshot path must not be empty");
        }

        let requested_backend = self.backend;
        let backend = requested_backend.resolve(BackendEnvironment::detect());
        info!(?requested_backend, ?backend, "selected Weld backend");
        let prepared = HostBuilder::new()
            .backend(backend.into())
            .launch(self.client)
            .screenshot(self.screenshot)
            .remote_debug_enabled(self.remote_debug.is_some())
            .prepare()?;

        let context = prepared.render_context();
        let mut app = App::new();
        app.insert_resource(backend);
        configure_rendering(&mut app, context);
        app.add_plugins((
            DebugProtocolPlugin,
            WeldAppPlugin::new(context.extent, context.scale_factor),
        ));

        Ok(WeldApp {
            app,
            prepared,
            backend,
            remote_debug: self.remote_debug,
        })
    }
}

/// A configurable Bevy application attached to a prepared Weld host.
pub struct WeldApp {
    app: App,
    prepared: PreparedHost,
    backend: ActiveBackend,
    remote_debug: Option<String>,
}

impl WeldApp {
    /// Begins configuring a Weld-hosted Bevy application.
    pub fn builder() -> WeldAppBuilder {
        WeldAppBuilder::default()
    }

    /// Returns the native backend resolved during [`WeldAppBuilder::build`].
    pub const fn backend(&self) -> ActiveBackend {
        self.backend
    }

    /// Adds Bevy plugins to the hosted application.
    pub fn add_plugins<M>(&mut self, plugins: impl Plugins<M>) -> &mut Self {
        self.app.add_plugins(plugins);
        self
    }

    /// Adds Bevy systems to a schedule in the hosted application.
    pub fn add_systems<M>(
        &mut self,
        schedule: impl ScheduleLabel,
        systems: impl IntoScheduleConfigs<ScheduleSystem, M>,
    ) -> &mut Self {
        self.app.add_systems(schedule, systems);
        self
    }

    /// Inserts a Bevy resource into the hosted application.
    pub fn insert_resource<R: Resource>(&mut self, resource: R) -> &mut Self {
        self.app.insert_resource(resource);
        self
    }

    /// Borrows the underlying Bevy application.
    pub const fn app(&self) -> &App {
        &self.app
    }

    /// Mutably borrows the underlying Bevy application.
    pub const fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    /// Finalizes Bevy plugins and runs the prepared native host.
    ///
    /// This consumes the application and must be called on the thread that
    /// built it. Plugin finalization and cleanup run immediately before the
    /// native event loop starts.
    pub fn run(mut self) -> Result<()> {
        if let Some(address) = self.remote_debug.take() {
            configure_remote_debug(&mut self.app, &address)
                .context("failed to configure remote debugging")?;
        }
        let (context, runtime) = self.prepared.into_parts();
        let shell = AppShell::new(self.app, context)?;
        runtime.run(shell)
    }
}

impl Backend {
    fn resolve(self, environment: BackendEnvironment) -> ActiveBackend {
        match self {
            Self::Nested => ActiveBackend::Nested,
            Self::Drm => ActiveBackend::Drm,
            Self::Auto => resolve_auto_backend(environment),
        }
    }
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
            bare_vt_console: environment_equals("TERM", "linux"),
            wayland_host: inherited_wayland_socket_available() || wayland_display_available(),
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

fn resolve_auto_backend(environment: BackendEnvironment) -> ActiveBackend {
    if environment.bare_vt_console {
        return ActiveBackend::Drm;
    }
    if environment.wayland_host || environment.x11_host || environment.graphical_session {
        return ActiveBackend::Nested;
    }
    if environment.tty_session || environment.virtual_terminal {
        return ActiveBackend::Drm;
    }
    ActiveBackend::Nested
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
            ActiveBackend::Nested
        );

        let x11_host = BackendEnvironment {
            x11_host: true,
            ..Default::default()
        };
        assert_eq!(resolve_auto_backend(x11_host), ActiveBackend::Nested);

        let graphical_session = BackendEnvironment {
            graphical_session: true,
            ..Default::default()
        };
        assert_eq!(
            resolve_auto_backend(graphical_session),
            ActiveBackend::Nested
        );

        let bare_console_with_stale_graphical_environment = BackendEnvironment {
            bare_vt_console: true,
            wayland_host: true,
            x11_host: true,
            graphical_session: true,
            tty_session: true,
            virtual_terminal: true,
        };
        assert_eq!(
            resolve_auto_backend(bare_console_with_stale_graphical_environment),
            ActiveBackend::Drm
        );

        let tty = BackendEnvironment {
            tty_session: true,
            virtual_terminal: true,
            ..Default::default()
        };
        assert_eq!(resolve_auto_backend(tty), ActiveBackend::Drm);
        assert_eq!(
            resolve_auto_backend(BackendEnvironment::default()),
            ActiveBackend::Nested
        );
    }

    #[test]
    fn app_extension_distinguishes_plain_and_weld_apps() {
        let mut app = App::new();
        assert_eq!(app.backend(), None);
        app.insert_resource(ActiveBackend::Drm);
        assert_eq!(app.backend(), Some(ActiveBackend::Drm));
    }

    #[test]
    fn builder_keeps_bootstrap_options_until_preparation() {
        let builder = WeldApp::builder()
            .backend(Backend::Drm)
            .launch(["foot", "--maximized"])
            .screenshot(Some(PathBuf::from("frame.png")))
            .remote_debug(Some("127.0.0.1:15702".to_owned()));

        assert_eq!(builder.backend, Backend::Drm);
        assert_eq!(builder.client, ["foot", "--maximized"]);
        assert_eq!(builder.screenshot, Some(PathBuf::from("frame.png")));
        assert_eq!(builder.remote_debug.as_deref(), Some("127.0.0.1:15702"));
    }
}
