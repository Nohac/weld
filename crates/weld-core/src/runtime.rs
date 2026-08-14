//! Shared host-runtime policy and process lifecycle.

use std::{
    collections::VecDeque,
    ffi::{OsStr, OsString},
    path::PathBuf,
    process::{Child, Command},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};

use crate::server::ServerState;

pub(crate) const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
pub(crate) const REMOTE_DEBUG_MAINTENANCE_INTERVAL: Duration = Duration::from_millis(100);
pub(crate) const CAPTURE_DEADLINE: Duration = Duration::from_secs(10);
const INACTIVE_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const BEVY_SETTLE_COMPOSITIONS: u8 = 5;

/// Data borrowed by calloop callbacks without making Smithay the owner of
/// backend events or process policy.
pub(crate) struct LoopData<Event, BackendState = ()> {
    pub(crate) server: ServerState,
    pub(crate) events: VecDeque<Event>,
    pub(crate) backend_state: BackendState,
}

impl<Event> LoopData<Event> {
    pub(crate) fn new(server: ServerState) -> Self {
        Self {
            server,
            events: VecDeque::new(),
            backend_state: (),
        }
    }
}

impl<Event, BackendState> LoopData<Event, BackendState> {
    pub(crate) fn with_state(server: ServerState, backend_state: BackendState) -> Self {
        Self {
            server,
            events: VecDeque::new(),
            backend_state,
        }
    }
}

pub(crate) fn server_mut<Event, BackendState>(
    data: &mut LoopData<Event, BackendState>,
) -> &mut ServerState {
    &mut data.server
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputScaleAdjustment {
    Increase,
    Decrease,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostCommand {
    Launch {
        program: OsString,
        arguments: Vec<OsString>,
    },
    AdjustOutputScale(OutputScaleAdjustment),
    Exit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostCommandEffect {
    Continue,
    Exit,
    AdjustOutputScale(OutputScaleAdjustment),
}

#[derive(Default)]
pub(crate) struct ChildProcesses(Vec<Child>);

impl ChildProcesses {
    pub(crate) fn spawn_requested(
        &mut self,
        server: &ServerState,
        arguments: &[OsString],
    ) -> Result<bool> {
        let Some((program, arguments)) = arguments.split_first() else {
            return Ok(false);
        };
        self.spawn(server, program, arguments)?;
        Ok(true)
    }

    pub(crate) fn apply(
        &mut self,
        server: &ServerState,
        command: HostCommand,
    ) -> Result<HostCommandEffect> {
        match command {
            HostCommand::Launch { program, arguments } => {
                self.spawn(server, &program, &arguments)?;
                Ok(HostCommandEffect::Continue)
            }
            HostCommand::AdjustOutputScale(adjustment) => {
                Ok(HostCommandEffect::AdjustOutputScale(adjustment))
            }
            HostCommand::Exit => Ok(HostCommandEffect::Exit),
        }
    }

    pub(crate) fn reap(&mut self) {
        self.0.retain_mut(|process| {
            process
                .try_wait()
                .map(|status| status.is_none())
                .unwrap_or(true)
        });
    }

    fn spawn(
        &mut self,
        server: &ServerState,
        program: &OsStr,
        arguments: &[OsString],
    ) -> Result<()> {
        let _launch_span =
            tracing::trace_span!(target: crate::PROFILE_TARGET, "host_launch_client").entered();
        let mut command = Command::new(program);
        command.args(arguments);
        configure_client_command(&mut command, &server.socket_name);
        let child = command
            .spawn()
            .with_context(|| format!("failed to spawn Wayland client {program:?}"))?;
        tracing::trace!(
            target: crate::PROFILE_TARGET,
            ?program,
            process_id = child.id(),
            "launched Wayland client"
        );
        self.0.push(child);
        Ok(())
    }
}

/// Keep this environment exactly synchronized with `scripts/run-app`.
pub(crate) fn configure_client_command(command: &mut Command, socket_name: &OsStr) {
    command
        .env("WAYLAND_DISPLAY", socket_name)
        .env("GDK_BACKEND", "wayland")
        .env("QT_QPA_PLATFORM", "wayland")
        .env("SDL_VIDEODRIVER", "wayland")
        .env("SDL_VIDEO_DRIVER", "wayland")
        .env("MOZ_ENABLE_WAYLAND", "1")
        .env("NIXOS_OZONE_WL", "1")
        .env("XDG_SESSION_TYPE", "wayland")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_SOCKET");
}

#[derive(Debug)]
pub(crate) struct FrameState {
    composition_dirty: bool,
    settle_compositions_remaining: u8,
    present_needed: bool,
    next_composition: Option<Instant>,
    frame_interval: Duration,
}

impl Default for FrameState {
    fn default() -> Self {
        Self {
            composition_dirty: true,
            // Bevy's own winit runner forces five startup updates because
            // plugin startup, layout, extraction, and GPU asset preparation
            // need not settle in one pass. Weld drives those stages manually.
            settle_compositions_remaining: BEVY_SETTLE_COMPOSITIONS,
            present_needed: true,
            next_composition: None,
            frame_interval: FRAME_INTERVAL,
        }
    }
}

impl FrameState {
    pub(crate) fn with_refresh_millihertz(mut self, refresh_millihertz: u32) -> Self {
        if refresh_millihertz > 0 {
            self.frame_interval = Duration::from_nanos(
                (1_000_000_000_000_u64 / u64::from(refresh_millihertz))
                    .clamp(1_000_000, 100_000_000),
            );
        }
        self
    }

    #[cfg(test)]
    pub(crate) const fn composition_dirty(&self) -> bool {
        self.composition_dirty
    }

    #[cfg(test)]
    pub(crate) const fn present_needed(&self) -> bool {
        self.present_needed
    }

    #[cfg(test)]
    pub(crate) const fn settle_compositions_remaining(&self) -> u8 {
        self.settle_compositions_remaining
    }

    pub(crate) const fn presentation_due(&self) -> bool {
        self.present_needed && !self.composition_dirty
    }

    pub(crate) fn request_composition(&mut self) {
        self.composition_dirty = true;
    }

    /// Request enough paced compositions for deferred Bevy work to settle.
    ///
    /// Structural changes such as newly mapped surface trees can span main
    /// schedules, render extraction, asset preparation, and the GPU queue.
    pub(crate) fn request_settled_composition(&mut self) {
        self.composition_dirty = true;
        self.settle_compositions_remaining = BEVY_SETTLE_COMPOSITIONS;
    }

    pub(crate) fn request_present(&mut self) {
        self.present_needed = true;
    }

    pub(crate) fn composition_due(&self, now: Instant) -> bool {
        self.composition_pending() && self.next_composition.is_none_or(|deadline| deadline <= now)
    }

    pub(crate) fn composition_timeout(&self, now: Instant, session_active: bool) -> Duration {
        if !session_active {
            return INACTIVE_MAINTENANCE_INTERVAL;
        }
        if !self.composition_pending() {
            return self.frame_interval;
        }
        self.next_composition
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or(Duration::ZERO)
            .min(self.frame_interval)
    }

    pub(crate) fn composition_demand_timeout(
        &self,
        now: Instant,
        session_active: bool,
    ) -> Option<Duration> {
        (session_active && self.composition_pending()).then(|| {
            self.next_composition
                .map(|deadline| deadline.saturating_duration_since(now))
                .unwrap_or(Duration::ZERO)
                .min(self.frame_interval)
        })
    }

    pub(crate) fn composition_rendered(&mut self, now: Instant) {
        self.composition_dirty = false;
        self.settle_compositions_remaining = self.settle_compositions_remaining.saturating_sub(1);
        self.present_needed = true;
        self.next_composition = Some(now + self.frame_interval);
    }

    pub(crate) fn presented(&mut self) {
        self.present_needed = false;
    }

    const fn composition_pending(&self) -> bool {
        self.composition_dirty || self.settle_compositions_remaining > 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IterationWork {
    pub(crate) advance_main: bool,
}

pub(crate) const fn iteration_work(composition_due: bool, session_active: bool) -> IterationWork {
    IterationWork {
        advance_main: session_active && composition_due,
    }
}

pub(crate) struct PendingCapture {
    pub(crate) path: PathBuf,
    pub(crate) remote_request_id: Option<u64>,
    pub(crate) deadline: Instant,
    pub(crate) wait_for_client: bool,
}

impl PendingCapture {
    pub(crate) fn startup(path: PathBuf, wait_for_client: bool) -> Self {
        Self {
            path,
            remote_request_id: None,
            deadline: Instant::now() + CAPTURE_DEADLINE,
            wait_for_client,
        }
    }

    pub(crate) fn remote(request_id: u64, path: PathBuf) -> Self {
        Self {
            path,
            remote_request_id: Some(request_id),
            deadline: Instant::now() + CAPTURE_DEADLINE,
            wait_for_client: false,
        }
    }

    pub(crate) const fn is_startup(&self) -> bool {
        self.remote_request_id.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_session_does_not_poll_an_overdue_composition() {
        let started_at = Instant::now();
        let mut frame = FrameState::default();
        frame.composition_rendered(started_at);
        frame.request_composition();
        let overdue = started_at + FRAME_INTERVAL;

        assert_eq!(frame.composition_timeout(overdue, true), Duration::ZERO);
        assert_eq!(
            frame.composition_timeout(overdue, false),
            INACTIVE_MAINTENANCE_INTERVAL
        );
    }

    #[test]
    fn output_refresh_drives_pacing_with_defensive_bounds() {
        let now = Instant::now();
        let mut high_refresh = FrameState::default().with_refresh_millihertz(120_000);
        high_refresh.composition_rendered(now);
        assert_eq!(
            high_refresh.composition_timeout(now, true),
            Duration::from_nanos(8_333_333)
        );

        let mut implausibly_slow = FrameState::default().with_refresh_millihertz(1);
        implausibly_slow.composition_rendered(now);
        assert_eq!(
            implausibly_slow.composition_timeout(now, true),
            Duration::from_millis(100)
        );
    }
}
