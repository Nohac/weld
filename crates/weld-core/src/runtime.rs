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
pub(crate) const BEVY_SETTLE_COMPOSITIONS: u8 = 5;

/// Data borrowed by calloop callbacks without making Smithay the owner of
/// backend events or process policy.
pub(crate) struct LoopData<Event> {
    pub(crate) server: ServerState,
    pub(crate) events: VecDeque<Event>,
}

impl<Event> LoopData<Event> {
    pub(crate) fn new(server: ServerState) -> Self {
        Self {
            server,
            events: VecDeque::new(),
        }
    }
}

pub(crate) fn server_mut<Event>(data: &mut LoopData<Event>) -> &mut ServerState {
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
    MatchOutputPhysicalScale,
    Exit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostCommandEffect {
    Continue,
    Exit,
    AdjustOutputScale(OutputScaleAdjustment),
    MatchOutputPhysicalScale,
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
            HostCommand::MatchOutputPhysicalScale => {
                Ok(HostCommandEffect::MatchOutputPhysicalScale)
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
    update_dirty: bool,
    composition_dirty: bool,
    settle_compositions_remaining: u8,
    present_needed: bool,
    next_composition: Option<Instant>,
    frame_interval: Duration,
}

impl Default for FrameState {
    fn default() -> Self {
        Self {
            update_dirty: true,
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
    #[cfg(test)]
    pub(crate) const fn update_dirty(&self) -> bool {
        self.update_dirty
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

    pub(crate) fn request_update(&mut self) {
        self.update_dirty = true;
    }

    pub(crate) fn request_composition(&mut self) {
        self.update_dirty = true;
        self.composition_dirty = true;
    }

    /// Request enough paced compositions for deferred Bevy work to settle.
    ///
    /// Structural changes such as newly mapped surface trees can span main
    /// schedules, render extraction, asset preparation, and the GPU queue.
    pub(crate) fn request_settled_composition(&mut self) {
        self.update_dirty = true;
        self.composition_dirty = true;
        self.settle_compositions_remaining = BEVY_SETTLE_COMPOSITIONS;
    }

    pub(crate) fn request_present(&mut self) {
        self.present_needed = true;
    }

    pub(crate) fn update_due(&self, now: Instant) -> bool {
        self.update_dirty && self.next_composition.is_none_or(|deadline| deadline <= now)
    }

    pub(crate) fn composition_due(&self, now: Instant) -> bool {
        self.composition_pending() && self.next_composition.is_none_or(|deadline| deadline <= now)
    }

    pub(crate) fn composition_timeout(&self, now: Instant) -> Duration {
        if !self.work_pending() {
            return self.frame_interval;
        }
        self.next_composition
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or(Duration::ZERO)
            .min(self.frame_interval)
    }

    pub(crate) fn application_advanced(&mut self, now: Instant) {
        self.update_dirty = false;
        self.next_composition = Some(now + self.frame_interval);
    }

    pub(crate) fn composition_rendered(&mut self, now: Instant) {
        self.update_dirty = false;
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

    const fn work_pending(&self) -> bool {
        self.update_dirty || self.composition_pending()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IterationWork {
    pub(crate) advance_main: bool,
    pub(crate) render_composition: bool,
}

pub(crate) const fn iteration_work(update_due: bool, composition_due: bool) -> IterationWork {
    IterationWork {
        advance_main: update_due || composition_due,
        render_composition: composition_due,
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
