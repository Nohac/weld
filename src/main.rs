//! Nested validation host for Weld's Smithay, Bevy, and wgpu boundary.

mod debug;
mod renderer;
mod server;
mod shell;

use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Child, Command},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use renderer::NestedRenderer;
use server::ServerState;
use shell::ShellRenderer;
use smithay::reexports::{calloop::EventLoop as CalloopEventLoop, wayland_server::Display};
use tracing::{info, warn};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalSize},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    platform::pump_events::{EventLoopExtPumpEvents, PumpStatus},
    window::{Window, WindowAttributes, WindowId},
};

const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
const CAPTURE_DEADLINE: Duration = Duration::from_secs(10);
const DEFAULT_REMOTE_ADDRESS: &str = "127.0.0.1:15702";

#[derive(Parser)]
#[command(
    version,
    about = "Nested compositor validation host",
    trailing_var_arg = true
)]
struct AppArguments {
    /// Enable the restricted Bevy Remote Protocol endpoint.
    #[arg(
        long,
        value_name = "HOST:PORT",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = DEFAULT_REMOTE_ADDRESS
    )]
    remote_debug: Option<String>,

    /// Capture the first settled composition and exit.
    #[arg(long, value_name = "PATH")]
    screenshot: Option<PathBuf>,

    /// Optional nested client program followed by its arguments.
    #[arg(value_name = "CLIENT_AND_ARGS", allow_hyphen_values = true)]
    client: Vec<OsString>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init()
        .map_err(|error| anyhow!("failed to initialize tracing: {error}"))?;
    let arguments = AppArguments::parse();
    if arguments
        .screenshot
        .as_ref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
        bail!("--screenshot requires a non-empty path");
    }
    let mut host_event_loop =
        EventLoop::new().context("failed to create the nested host event loop")?;
    host_event_loop.set_control_flow(ControlFlow::Poll);
    let mut host = NestedHost::default();
    let initial_status = host_event_loop.pump_app_events(None, &mut host);
    if matches!(initial_status, PumpStatus::Exit(_)) {
        if arguments.screenshot.is_some() {
            bail!("nested host exited before the startup screenshot could be captured");
        }
        return Ok(());
    }
    let window = host
        .window
        .clone()
        .context("nested host did not create a window")?;
    if let Some(error) = host.creation_error.take() {
        bail!("failed to create the nested window: {error}");
    }

    let initial_size = nonzero_size(window.inner_size());
    let display_handle = host_event_loop.owned_display_handle();
    let mut renderer = NestedRenderer::new(window, display_handle, initial_size)?;
    let mut shell = ShellRenderer::new(
        renderer.instance(),
        renderer.adapter(),
        renderer.device(),
        renderer.queue(),
        initial_size.width,
        initial_size.height,
        arguments.remote_debug.as_deref(),
    )?;

    let mut calloop: CalloopEventLoop<'static, ServerState> =
        CalloopEventLoop::try_new().context("failed to create the Smithay calloop event loop")?;
    let display = Display::<ServerState>::new().context("failed to create the Wayland display")?;
    let mut server = ServerState::new(&mut calloop, display)?;
    let mut child = spawn_requested_client(&server, &arguments.client)?;
    let child_requested = !arguments.client.is_empty();
    let mut pending_capture = arguments
        .screenshot
        .map(|path| PendingCapture::startup(path, child_requested));

    info!(socket = ?server.socket_name, "Weld nested compositor is ready");
    loop {
        let host_exited = matches!(
            host_event_loop.pump_app_events(Some(Duration::ZERO), &mut host),
            PumpStatus::Exit(_)
        ) || host.close_requested;
        if host_exited {
            if pending_capture
                .as_ref()
                .is_some_and(PendingCapture::is_startup)
            {
                bail!("nested host closed before the startup screenshot completed");
            }
            break;
        }

        if let Some(size) = host.pending_size.take()
            && size.width > 0
            && size.height > 0
        {
            renderer.resize(size);
            shell.resize(size.width, size.height);
        }

        calloop
            .dispatch(Some(FRAME_INTERVAL), &mut server)
            .context("Smithay calloop dispatch failed")?;
        if let Some(update) = server.take_surface_update() {
            renderer.apply_surface_update(update);
        }

        shell.update();
        if shell.should_exit() {
            if pending_capture
                .as_ref()
                .is_some_and(PendingCapture::is_startup)
            {
                bail!("Bevy exited before the startup screenshot completed");
            }
            break;
        }

        if pending_capture.is_none()
            && let Some(request) = shell.take_capture_request()
        {
            pending_capture = Some(PendingCapture::remote(request.request_id, request.path));
        }
        if pending_capture
            .as_ref()
            .is_some_and(|capture| capture.deadline <= Instant::now())
            && let Some(capture) = pending_capture.take()
        {
            let error = "screenshot timed out before a presentable frame was available".to_owned();
            if let Some(request_id) = capture.remote_request_id {
                shell.complete_capture(request_id, Err(error));
            } else {
                bail!("startup {error}");
            }
        }

        let capture_path = pending_capture.as_ref().and_then(|capture| {
            let client_ready = !capture.wait_for_client || renderer.has_client_frame();
            client_ready.then_some(capture.path.as_path())
        });
        let frame = renderer.render(shell.texture_view(), capture_path)?;
        if frame.presented {
            server.frame_presented();
        }
        if let Some(capture_result) = frame.capture
            && let Some(capture) = pending_capture.take()
        {
            match capture.remote_request_id {
                Some(request_id) => {
                    if let Err(error) = &capture_result {
                        warn!(request_id, %error, "remote screenshot failed");
                    }
                    shell.complete_capture(request_id, capture_result);
                }
                None => match capture_result {
                    Ok(()) => {
                        info!(path = %capture.path.display(), "startup screenshot saved");
                        return Ok(());
                    }
                    Err(error) => bail!("startup screenshot failed: {error}"),
                },
            }
        }
        server.flush_clients();

        if child
            .as_mut()
            .is_some_and(|process| process.try_wait().ok().flatten().is_some())
        {
            child = None;
        }
    }

    drop(child);
    Ok(())
}

struct PendingCapture {
    path: PathBuf,
    remote_request_id: Option<u64>,
    deadline: Instant,
    wait_for_client: bool,
}

impl PendingCapture {
    fn startup(path: PathBuf, wait_for_client: bool) -> Self {
        Self {
            path,
            remote_request_id: None,
            deadline: Instant::now() + CAPTURE_DEADLINE,
            wait_for_client,
        }
    }

    fn remote(request_id: u64, path: PathBuf) -> Self {
        Self {
            path,
            remote_request_id: Some(request_id),
            deadline: Instant::now() + CAPTURE_DEADLINE,
            wait_for_client: false,
        }
    }

    const fn is_startup(&self) -> bool {
        self.remote_request_id.is_none()
    }
}

fn spawn_requested_client(server: &ServerState, arguments: &[OsString]) -> Result<Option<Child>> {
    let Some((program, arguments)) = arguments.split_first() else {
        return Ok(None);
    };
    let child = Command::new(program)
        .args(arguments)
        .env("WAYLAND_DISPLAY", &server.socket_name)
        .spawn()
        .with_context(|| format!("failed to spawn nested client {program:?}"))?;
    Ok(Some(child))
}

fn nonzero_size(size: PhysicalSize<u32>) -> PhysicalSize<u32> {
    PhysicalSize::new(size.width.max(1), size.height.max(1))
}

#[derive(Default)]
struct NestedHost {
    window: Option<Arc<Window>>,
    pending_size: Option<PhysicalSize<u32>>,
    creation_error: Option<String>,
    close_requested: bool,
}

impl ApplicationHandler for NestedHost {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.creation_error.is_some() {
            return;
        }
        let attributes = WindowAttributes::default()
            .with_title("Weld nested compositor")
            .with_inner_size(LogicalSize::new(960.0, 640.0));
        match event_loop.create_window(attributes) {
            Ok(window) => {
                let window = Arc::new(window);
                self.pending_size = Some(window.inner_size());
                self.window = Some(window);
            }
            Err(error) => self.creation_error = Some(error.to_string()),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                self.close_requested = true;
                event_loop.exit();
            }
            WindowEvent::Resized(size) => self.pending_size = Some(size),
            _ => {}
        }
    }
}
