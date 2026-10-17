//! Commit-scoped feedback transferred into the physical frame that samples it.

use std::time::Duration;

use smithay::{
    backend::drm::{DrmEventMetadata, DrmEventTime},
    output::Output,
    reexports::{
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind,
        wayland_server::{DisplayHandle, protocol::wl_surface::WlSurface},
    },
    utils::{Clock, Monotonic},
    wayland::{
        compositor::with_states,
        presentation::{PresentationFeedbackCachedState, PresentationState, Refresh},
    },
};

use super::ServerState;
use crate::{OutputId, host::SurfacePresentation};

pub(super) fn global(display: &DisplayHandle) -> PresentationState {
    PresentationState::new::<ServerState>(display, Clock::<Monotonic>::new().id() as u32)
}

#[derive(Debug)]
pub(crate) struct OutputFeedback {
    output: Output,
    commits: Vec<PresentationFeedbackCachedState>,
}

impl OutputFeedback {
    pub(crate) fn presented(mut self, timing: PresentationTiming) {
        tracing::trace!(target: "weld_presentation", output = %self.output.name(),
            callbacks = self.commits.iter().map(|commit| commit.callbacks.len()).sum::<usize>(),
            time = ?timing.time, refresh = ?timing.refresh, sequence = timing.sequence,
            flags = ?timing.flags, "completed presentation feedback");
        for commit in &mut self.commits {
            for callback in commit.callbacks.drain(..) {
                callback.presented(
                    &self.output,
                    timing.time,
                    timing.refresh,
                    timing.sequence,
                    timing.flags,
                );
            }
        }
    }
}

pub(crate) struct PresentationTiming {
    time: Duration,
    refresh: Refresh,
    sequence: u64,
    flags: Kind,
}

impl PresentationTiming {
    pub(crate) fn drm(metadata: Option<DrmEventMetadata>, interval: Duration) -> Self {
        let hardware_time = metadata.as_ref().and_then(|event| match event.time {
            DrmEventTime::Monotonic(time) if !time.is_zero() => Some(time),
            _ => None,
        });
        Self {
            time: hardware_time.unwrap_or_else(|| Clock::<Monotonic>::new().now().into()),
            refresh: Refresh::fixed(interval),
            sequence: metadata.map_or(0, |event| u64::from(event.sequence)),
            flags: if hardware_time.is_some() {
                Kind::Vsync | Kind::HwClock | Kind::HwCompletion
            } else {
                Kind::Vsync
            },
        }
    }

    pub(crate) fn nested() -> Self {
        Self {
            time: Clock::<Monotonic>::new().now().into(),
            refresh: Refresh::Unknown,
            sequence: 0,
            flags: Kind::empty(),
        }
    }
}

impl ServerState {
    pub(super) fn discard_surface_feedback(
        &self,
        surface: crate::surface::SurfaceId,
        root: &WlSurface,
    ) {
        self.pending_surface_events.discard_feedback(surface);
        for surface in super::surface_tree::collect_surfaces(root) {
            with_states(&surface, |states| {
                drop(std::mem::take(
                    &mut *states
                        .cached_state
                        .get::<PresentationFeedbackCachedState>()
                        .current(),
                ));
            });
        }
    }
    pub(super) fn publish_surface_snapshot(
        &self,
        surface: crate::surface::SurfaceId,
        mut snapshot: super::PendingSurfaceTreeSnapshot,
    ) {
        if !self.presentation_claims.native(surface) {
            snapshot.feedback.clear();
        }
        self.pending_surface_events
            .push_back(super::PendingSurfaceEvent {
                surface,
                kind: super::PendingSurfaceEventKind::TreeSnapshot(snapshot),
            });
    }
    pub(crate) fn take_presentation_feedback(
        &self,
        output: OutputId,
        surfaces: &[SurfacePresentation],
    ) -> Option<OutputFeedback> {
        let output = self.native_output(output)?;
        let commits = surfaces
            .iter()
            .filter(|sampled| self.presentation_claims.native(sampled.surface))
            .flat_map(|sampled| self.pending_surface_events.take_feedback(*sampled))
            .collect::<Vec<_>>();
        (!commits.is_empty()).then_some(OutputFeedback { output, commits })
    }
}
