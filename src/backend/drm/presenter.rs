//! Event-driven direct DRM presentation worker and bounded host handoff.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result};
use calloop::channel::Sender as CalloopSender;
use tracing::{debug, warn};

use crate::{
    renderer::{CompositionBlitter, CursorOverlay},
    shell::CompositionTargetId,
};

use super::direct::DirectDrmGpu;

const PRESENTER_GENERATION: u64 = 1;
const MAX_CONSECUTIVE_DEFERRED_FRAMES: u8 = 3;

#[derive(Debug)]
pub(super) enum PresenterEvent {
    Ready {
        epoch: u64,
    },
    FrameReleased {
        generation: u64,
        epoch: u64,
        frame_id: u64,
        target: CompositionTargetId,
        outcome: FrameOutcome,
    },
    OutputUnavailable(String),
    DeviceLost(String),
    UncapturedError(String),
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FrameOutcome {
    Presented,
    Deferred,
    Interrupted,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrameTicket {
    generation: u64,
    epoch: u64,
    frame_id: u64,
    target: CompositionTargetId,
}

#[derive(Clone)]
struct PendingFrame {
    ticket: FrameTicket,
    payload: PresentedComposition,
}

#[derive(Clone)]
struct PresentedComposition {
    // wgpu resources are reference-counted. Cloning this handle retains the
    // same GPU texture view; it never copies composition pixels.
    view: wgpu::TextureView,
    cursor: CursorOverlay,
}

struct FrameQueue<Payload> {
    in_flight: Option<(FrameTicket, Payload)>,
    pending: Option<(FrameTicket, Payload)>,
    next_frame_id: u64,
}

impl<Payload: Clone> FrameQueue<Payload> {
    const fn new() -> Self {
        Self {
            in_flight: None,
            pending: None,
            next_frame_id: 1,
        }
    }

    fn offer(
        &mut self,
        generation: u64,
        epoch: u64,
        target: CompositionTargetId,
        payload: Payload,
        submit_now: bool,
    ) -> Option<(FrameTicket, Payload)> {
        let ticket = FrameTicket {
            generation,
            epoch,
            frame_id: self.next_frame_id,
            target,
        };
        self.next_frame_id = self.next_frame_id.saturating_add(1);
        let frame = (ticket, payload);
        if submit_now && self.in_flight.is_none() {
            self.in_flight = Some(frame.clone());
            Some(frame)
        } else {
            self.pending = Some(frame);
            None
        }
    }

    fn release(&mut self, ticket: FrameTicket, outcome: FrameOutcome, current_epoch: u64) -> bool {
        let Some((in_flight_ticket, _)) = self.in_flight.as_ref() else {
            return false;
        };
        if *in_flight_ticket != ticket {
            return false;
        }
        let Some((mut released_ticket, payload)) = self.in_flight.take() else {
            return false;
        };
        if matches!(outcome, FrameOutcome::Deferred | FrameOutcome::Interrupted)
            && self.pending.is_none()
        {
            released_ticket.epoch = current_epoch;
            self.pending = Some((released_ticket, payload));
        }
        true
    }

    fn take_pending(&mut self) -> Option<(FrameTicket, Payload)> {
        if self.in_flight.is_some() {
            return None;
        }
        let frame = self.pending.take()?;
        self.in_flight = Some(frame.clone());
        Some(frame)
    }

    fn send_failed(&mut self, frame: (FrameTicket, Payload)) {
        self.in_flight = None;
        self.pending = Some(frame);
    }

    fn clear(&mut self) {
        self.in_flight = None;
        self.pending = None;
    }
}

enum PresenterCommand {
    Frame(PendingFrame),
    Configure { epoch: u64 },
    Suspend,
    Shutdown,
}

pub(super) struct PresenterHandle {
    commands: mpsc::Sender<PresenterCommand>,
    worker: Option<JoinHandle<()>>,
    epoch: Arc<AtomicU64>,
    active: Arc<AtomicBool>,
    ready: bool,
    frames: FrameQueue<PresentedComposition>,
    stopped: bool,
    shutdown_requested: bool,
}

impl PresenterHandle {
    pub(super) fn spawn(gpu: DirectDrmGpu, events: CalloopSender<PresenterEvent>) -> Result<Self> {
        let (commands, command_receiver) = mpsc::channel();
        let epoch = Arc::new(AtomicU64::new(1));
        let active = Arc::new(AtomicBool::new(true));
        let worker_epoch = Arc::clone(&epoch);
        let worker_active = Arc::clone(&active);

        let uncaptured_events = events.clone();
        gpu.device.on_uncaptured_error(Arc::new(move |error| {
            let _ = uncaptured_events.send(PresenterEvent::UncapturedError(error.to_string()));
        }));
        let device_lost_events = events.clone();
        gpu.device.set_device_lost_callback(move |reason, message| {
            let _ = device_lost_events
                .send(PresenterEvent::DeviceLost(format!("{reason:?}: {message}")));
        });

        let worker = thread::Builder::new()
            .name("weld-drm-presenter".to_owned())
            .spawn(move || {
                let stopped_events = events.clone();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run_worker(gpu, command_receiver, events, worker_epoch, worker_active);
                }));
                if result.is_err() {
                    let _ = stopped_events.send(PresenterEvent::OutputUnavailable(
                        "direct DRM presenter worker panicked".to_owned(),
                    ));
                }
                let _ = stopped_events.send(PresenterEvent::Stopped);
            })
            .context("failed to spawn the direct DRM presenter worker")?;

        Ok(Self {
            commands,
            worker: Some(worker),
            epoch,
            active,
            ready: false,
            frames: FrameQueue::new(),
            stopped: false,
            shutdown_requested: false,
        })
    }

    pub(super) fn in_flight_target(&self) -> Option<CompositionTargetId> {
        self.frames
            .in_flight
            .as_ref()
            .map(|(ticket, _)| ticket.target)
    }

    pub(super) fn offer(
        &mut self,
        target: CompositionTargetId,
        composition: wgpu::TextureView,
        cursor: CursorOverlay,
    ) {
        let submit_now = self.can_submit();
        if let Some(frame) = self.frames.offer(
            PRESENTER_GENERATION,
            self.epoch.load(Ordering::Acquire),
            target,
            PresentedComposition {
                view: composition,
                cursor,
            },
            submit_now,
        ) {
            self.send_frame(frame);
        }
    }

    pub(super) fn suspend(&mut self) {
        self.ready = false;
        self.active.store(false, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let _ = self.commands.send(PresenterCommand::Suspend);
    }

    pub(super) fn activate(&mut self) {
        self.ready = false;
        let epoch = self.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        self.active.store(true, Ordering::Release);
        let _ = self.commands.send(PresenterCommand::Configure { epoch });
    }

    pub(super) fn handle_event(&mut self, event: &PresenterEvent) {
        match event {
            PresenterEvent::Ready { epoch }
                if *epoch == self.epoch.load(Ordering::Acquire)
                    && self.active.load(Ordering::Acquire) =>
            {
                self.ready = true;
                self.submit_pending();
            }
            PresenterEvent::FrameReleased {
                generation,
                epoch,
                frame_id,
                target,
                outcome,
            } => {
                let ticket = FrameTicket {
                    generation: *generation,
                    epoch: *epoch,
                    frame_id: *frame_id,
                    target: *target,
                };
                if !self
                    .frames
                    .release(ticket, *outcome, self.epoch.load(Ordering::Acquire))
                {
                    debug!(
                        generation,
                        epoch,
                        frame_id,
                        ?target,
                        "ignored stale presenter frame result"
                    );
                    return;
                }
                if matches!(outcome, FrameOutcome::Unavailable) {
                    self.ready = false;
                }
                self.submit_pending();
            }
            PresenterEvent::OutputUnavailable(_) => {
                self.ready = false;
            }
            PresenterEvent::DeviceLost(_) => {
                self.ready = false;
                self.frames.clear();
            }
            PresenterEvent::Stopped => {
                self.ready = false;
                self.stopped = true;
                self.frames.clear();
            }
            PresenterEvent::UncapturedError(_) => {}
            PresenterEvent::Ready { .. } => {}
        }
    }

    pub(super) const fn stopped(&self) -> bool {
        self.stopped
    }

    pub(super) fn begin_shutdown(&mut self) {
        if self.shutdown_requested {
            return;
        }
        self.shutdown_requested = true;
        self.ready = false;
        self.active.store(false, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let _ = self.commands.send(PresenterCommand::Shutdown);
    }

    pub(super) fn join_if_finished(&mut self) {
        if (self.stopped || self.worker.as_ref().is_some_and(JoinHandle::is_finished))
            && let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            warn!("direct DRM presenter worker panicked during shutdown");
        }
    }

    fn can_submit(&self) -> bool {
        self.ready
            && self.active.load(Ordering::Acquire)
            && self.frames.in_flight.is_none()
            && self
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
    }

    fn send_frame(&mut self, frame: (FrameTicket, PresentedComposition)) {
        let (ticket, payload) = frame;
        let command = PresenterCommand::Frame(PendingFrame {
            ticket,
            payload: payload.clone(),
        });
        if self.commands.send(command).is_err() {
            self.ready = false;
            self.frames.send_failed((ticket, payload));
        }
    }

    fn submit_pending(&mut self) {
        if self.can_submit()
            && let Some(frame) = self.frames.take_pending()
        {
            self.send_frame(frame);
        }
    }
}

impl Drop for PresenterHandle {
    fn drop(&mut self) {
        self.begin_shutdown();
        self.join_if_finished();
    }
}

fn run_worker(
    gpu: DirectDrmGpu,
    commands: mpsc::Receiver<PresenterCommand>,
    events: CalloopSender<PresenterEvent>,
    epoch: Arc<AtomicU64>,
    active: Arc<AtomicBool>,
) {
    let blitter = CompositionBlitter::new(&gpu.device, gpu.surface_config.format);
    let initial_epoch = epoch.load(Ordering::Acquire);
    if configure_surface(&gpu.surface, &gpu.device, &gpu.surface_config).is_err() {
        let _ = events.send(PresenterEvent::OutputUnavailable(
            "initial direct DRM surface configuration failed".to_owned(),
        ));
        return;
    }
    let _ = events.send(PresenterEvent::Ready {
        epoch: initial_epoch,
    });
    let frame_presenter = FramePresenter {
        surface: &gpu.surface,
        device: &gpu.device,
        queue: &gpu.queue,
        surface_config: &gpu.surface_config,
        blitter: &blitter,
        epoch: &epoch,
        active: &active,
    };
    let mut consecutive_deferred_frames = 0_u8;

    while let Ok(command) = commands.recv() {
        match command {
            PresenterCommand::Frame(frame) => {
                let mut outcome = frame_presenter.present(&frame);
                match outcome {
                    FrameOutcome::Presented => consecutive_deferred_frames = 0,
                    FrameOutcome::Deferred => {
                        consecutive_deferred_frames = consecutive_deferred_frames.saturating_add(1);
                        if consecutive_deferred_frames >= MAX_CONSECUTIVE_DEFERRED_FRAMES {
                            outcome = FrameOutcome::Unavailable;
                            let _ = events.send(PresenterEvent::OutputUnavailable(format!(
                                "direct DRM surface deferred {consecutive_deferred_frames} consecutive frames"
                            )));
                            consecutive_deferred_frames = 0;
                        }
                    }
                    FrameOutcome::Interrupted => consecutive_deferred_frames = 0,
                    FrameOutcome::Unavailable => {
                        let _ = events.send(PresenterEvent::OutputUnavailable(
                            "direct DRM surface is unavailable".to_owned(),
                        ));
                    }
                }
                release(&events, frame, outcome);
            }
            PresenterCommand::Configure {
                epoch: configure_epoch,
            } => {
                if configure_surface(&gpu.surface, &gpu.device, &gpu.surface_config).is_ok() {
                    consecutive_deferred_frames = 0;
                    let _ = events.send(PresenterEvent::Ready {
                        epoch: configure_epoch,
                    });
                } else {
                    let _ = events.send(PresenterEvent::OutputUnavailable(
                        "direct DRM surface reconfiguration failed".to_owned(),
                    ));
                }
            }
            PresenterCommand::Suspend => {}
            PresenterCommand::Shutdown => break,
        }
    }
}

struct FramePresenter<'a> {
    surface: &'a wgpu::Surface<'a>,
    device: &'a wgpu::Device,
    queue: &'a wgpu::Queue,
    surface_config: &'a wgpu::SurfaceConfiguration,
    blitter: &'a CompositionBlitter,
    epoch: &'a AtomicU64,
    active: &'a AtomicBool,
}

impl FramePresenter<'_> {
    fn present(&self, frame: &PendingFrame) -> FrameOutcome {
        use wgpu::CurrentSurfaceTexture;

        if !self.active.load(Ordering::Acquire)
            || self.epoch.load(Ordering::Acquire) != frame.ticket.epoch
        {
            return FrameOutcome::Interrupted;
        }
        let current = self.surface.get_current_texture();
        if !self.active.load(Ordering::Acquire)
            || self.epoch.load(Ordering::Acquire) != frame.ticket.epoch
        {
            if let CurrentSurfaceTexture::Success(texture)
            | CurrentSurfaceTexture::Suboptimal(texture) = current
            {
                drop(texture);
            }
            return FrameOutcome::Interrupted;
        }
        let (surface_texture, suboptimal) = match current {
            CurrentSurfaceTexture::Success(texture) => (texture, false),
            CurrentSurfaceTexture::Suboptimal(texture) => (texture, true),
            CurrentSurfaceTexture::Timeout | CurrentSurfaceTexture::Occluded => {
                return FrameOutcome::Deferred;
            }
            CurrentSurfaceTexture::Outdated => {
                return if configure_surface(self.surface, self.device, self.surface_config).is_ok()
                {
                    FrameOutcome::Deferred
                } else {
                    FrameOutcome::Unavailable
                };
            }
            CurrentSurfaceTexture::Lost | CurrentSurfaceTexture::Validation => {
                return FrameOutcome::Unavailable;
            }
        };
        let output = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        // FrameQueue permits only one worker-owned submission at a time, so
        // this shared uniform is rewritten immediately before the matching
        // queue submission and cannot be overtaken by a later cursor payload.
        self.blitter.set_cursor(self.queue, frame.payload.cursor);
        let bind_group = self.blitter.create_bind_group(
            self.device,
            "weld direct DRM composition bind group",
            &frame.payload.view,
        );
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("weld direct DRM composition encoder"),
            });
        self.blitter.encode(
            &mut encoder,
            "weld direct DRM composition pass",
            &output,
            &bind_group,
        );
        self.queue.submit([encoder.finish()]);
        surface_texture.present();
        if suboptimal && configure_surface(self.surface, self.device, self.surface_config).is_err()
        {
            return FrameOutcome::Unavailable;
        }
        FrameOutcome::Presented
    }
}

fn release(events: &CalloopSender<PresenterEvent>, frame: PendingFrame, outcome: FrameOutcome) {
    let _ = events.send(PresenterEvent::FrameReleased {
        generation: frame.ticket.generation,
        epoch: frame.ticket.epoch,
        frame_id: frame.ticket.frame_id,
        target: frame.ticket.target,
        outcome,
    });
}

fn configure_surface(
    surface: &wgpu::Surface<'_>,
    device: &wgpu::Device,
    config: &wgpu::SurfaceConfiguration,
) -> Result<(), ()> {
    match catch_unwind(AssertUnwindSafe(|| surface.configure(device, config))) {
        Ok(()) => Ok(()),
        Err(_) => {
            warn!("wgpu panicked while configuring the direct DRM surface");
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_presenter_keeps_only_the_latest_host_frame() {
        let mut frames = FrameQueue::new();
        let first = frames
            .offer(1, 1, CompositionTargetId::FIRST, "first", true)
            .expect("idle queue should submit its first frame");
        assert!(
            frames
                .offer(1, 1, CompositionTargetId::SECOND, "older", false)
                .is_none()
        );
        assert!(
            frames
                .offer(1, 1, CompositionTargetId::SECOND, "latest", false)
                .is_none()
        );

        assert!(frames.release(first.0, FrameOutcome::Presented, 1));
        let next = frames
            .take_pending()
            .expect("latest coalesced frame should become available");
        assert_eq!(next.1, "latest");
        assert_eq!(next.0.target, CompositionTargetId::SECOND);
    }

    #[test]
    fn stale_result_cannot_release_current_target_ownership() {
        let mut frames = FrameQueue::new();
        let current = frames
            .offer(1, 4, CompositionTargetId::FIRST, (), true)
            .expect("idle queue should submit its first frame");
        let stale = FrameTicket {
            frame_id: current.0.frame_id.saturating_add(1),
            ..current.0
        };

        assert!(!frames.release(stale, FrameOutcome::Presented, 4));
        assert_eq!(
            frames.in_flight.as_ref().map(|(ticket, _)| *ticket),
            Some(current.0)
        );
    }
}
