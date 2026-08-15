//! Event-driven GBM/KMS presentation with a bounded host/worker handoff.

use std::{
    collections::{HashMap, hash_map::Entry},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result, bail};
use ash::vk;
use calloop::channel::Sender as CalloopSender;
use smithay::{
    backend::{
        allocator::{
            Buffer as _, Fourcc, Modifier,
            dmabuf::Dmabuf,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{DrmDeviceFd, DrmError, DrmSurface, GbmBufferedSurface, GbmBufferedSurfaceError},
    },
    reexports::{drm::control::crtc, rustix::fs::fstat},
};
use tracing::{debug, warn};

use crate::{
    host::CompositionTargetView,
    renderer::{CursorOverlay, CursorOverlayRenderer},
    surface::Extent,
};

use super::gpu::DrmGpu;

const PRESENTER_GENERATION: u64 = 1;
const TRANSIENT_RECOVERY_ATTEMPTS: u8 = 3;
const SCANOUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;

type ScanoutSurface = GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, FrameTicket>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresenterState {
    Starting,
    Ready,
    Suspended,
    ActivatingAfterSession,
    Unavailable,
    Stopped,
}

struct PresenterLifecycle {
    state: PresenterState,
    recovery_attempts_remaining: u8,
}

impl PresenterLifecycle {
    const fn new() -> Self {
        Self {
            state: PresenterState::Starting,
            recovery_attempts_remaining: TRANSIENT_RECOVERY_ATTEMPTS,
        }
    }

    const fn is_stopped(&self) -> bool {
        matches!(self.state, PresenterState::Stopped)
    }

    fn worker_became_idle(&mut self) -> bool {
        if self.state != PresenterState::ActivatingAfterSession {
            return false;
        }
        self.state = PresenterState::Suspended;
        true
    }

    fn mark_ready(&mut self) {
        self.state = PresenterState::Ready;
    }

    fn suspend(&mut self) {
        self.state = PresenterState::Suspended;
    }

    fn begin_activation(&mut self, worker_is_rendering: bool) -> bool {
        self.recovery_attempts_remaining = TRANSIENT_RECOVERY_ATTEMPTS;
        if worker_is_rendering {
            self.state = PresenterState::ActivatingAfterSession;
            false
        } else {
            true
        }
    }

    fn begin_transient_recovery(&mut self) -> bool {
        self.state = PresenterState::Unavailable;
        if self.recovery_attempts_remaining == 0 {
            return false;
        }
        self.recovery_attempts_remaining -= 1;
        true
    }

    fn reset_recovery_budget(&mut self) {
        self.recovery_attempts_remaining = TRANSIENT_RECOVERY_ATTEMPTS;
    }

    fn disable(&mut self) {
        self.state = PresenterState::Unavailable;
        self.recovery_attempts_remaining = 0;
    }

    fn stop(&mut self) {
        self.state = PresenterState::Stopped;
    }
}

#[derive(Debug)]
pub(super) enum PresenterEvent {
    Ready { epoch: u64 },
    Frame(PresenterFrameEvent),
    OutputUnavailable(String),
    DeviceLost(String),
    UncapturedError(String),
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FrameOutcome {
    Interrupted,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PresenterFrameEvent {
    ticket: FrameTicket,
    kind: PresenterFrameEventKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresenterFrameEventKind {
    Prepared,
    Released(FrameOutcome),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrameTicket {
    generation: u64,
    epoch: u64,
    frame_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveFramePhase {
    Rendering,
    AwaitingVblank,
}

struct ActiveFrame {
    ticket: FrameTicket,
    phase: ActiveFramePhase,
}

struct FrameTracker {
    active: Option<ActiveFrame>,
    next_frame_id: u64,
}

impl FrameTracker {
    const fn new() -> Self {
        Self {
            active: None,
            next_frame_id: 1,
        }
    }

    fn begin(&mut self, generation: u64, epoch: u64) -> Option<FrameTicket> {
        if self.active.is_some() {
            return None;
        }
        let ticket = FrameTicket {
            generation,
            epoch,
            frame_id: self.next_frame_id,
        };
        self.next_frame_id = self.next_frame_id.saturating_add(1);
        self.active = Some(ActiveFrame {
            ticket,
            phase: ActiveFramePhase::Rendering,
        });
        Some(ticket)
    }

    fn mark_awaiting_vblank(&mut self, ticket: FrameTicket) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        if active.ticket != ticket || active.phase != ActiveFramePhase::Rendering {
            return false;
        }
        active.phase = ActiveFramePhase::AwaitingVblank;
        true
    }

    fn release_rendering(&mut self, ticket: FrameTicket) -> bool {
        self.release(ticket, ActiveFramePhase::Rendering)
    }

    fn release_vblank(&mut self, ticket: FrameTicket) -> bool {
        self.release(ticket, ActiveFramePhase::AwaitingVblank)
    }

    fn release(&mut self, ticket: FrameTicket, phase: ActiveFramePhase) -> bool {
        let Some(active) = self.active.as_ref() else {
            return false;
        };
        if active.ticket != ticket || active.phase != phase {
            return false;
        }
        self.active = None;
        true
    }

    fn clear(&mut self) {
        self.active = None;
    }

    fn suspend(&mut self) {
        self.drop_awaiting_vblank();
    }

    fn drop_awaiting_vblank(&mut self) {
        if self
            .active
            .as_ref()
            .is_some_and(|frame| frame.phase == ActiveFramePhase::AwaitingVblank)
        {
            self.active = None;
        }
    }

    fn has_rendering_frame(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|frame| frame.phase == ActiveFramePhase::Rendering)
    }

    fn is_rendering(&self, ticket: FrameTicket) -> bool {
        self.active.as_ref().is_some_and(|frame| {
            frame.ticket == ticket && frame.phase == ActiveFramePhase::Rendering
        })
    }
}

pub(super) struct AcquiredFrame {
    ticket: FrameTicket,
    target: CompositionTargetView,
    image: vk::Image,
}

impl AcquiredFrame {
    pub(super) const fn target(&self) -> &CompositionTargetView {
        &self.target
    }
}

struct CompletionWork {
    ticket: FrameTicket,
    submission: wgpu::SubmissionIndex,
    present: bool,
}

enum PresenterCommand {
    Wait(CompletionWork),
    Suspend,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PresenterTargetAvailability {
    Ready,
    Busy,
    Unavailable,
}

pub(super) struct PresenterHandle {
    commands: mpsc::Sender<PresenterCommand>,
    events: CalloopSender<PresenterEvent>,
    worker: Option<JoinHandle<()>>,
    epoch: Arc<AtomicU64>,
    active: Arc<AtomicBool>,
    crtc: crtc::Handle,
    scanout: ScanoutSurface,
    frames: FrameTracker,
    lifecycle: PresenterLifecycle,
    device: wgpu::Device,
    queue: wgpu::Queue,
    imports: ScanoutImportCache,
    cursor_renderer: CursorOverlayRenderer,
    shutdown_requested: bool,
}

impl PresenterHandle {
    pub(super) fn spawn(
        gpu: DrmGpu,
        drm_surface: DrmSurface,
        drm_fd: DrmDeviceFd,
        crtc: crtc::Handle,
        events: CalloopSender<PresenterEvent>,
    ) -> Result<Self> {
        let gbm = GbmDevice::new(drm_fd.clone()).context("failed to create GBM device")?;
        let allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
        let mut scanout = GbmBufferedSurface::new(
            drm_surface,
            allocator,
            &[Fourcc::Argb8888],
            gpu.renderable_scanout_formats.iter().copied(),
        )
        .context("failed to create Smithay GBM scanout surface")?;
        let (initial_scanout, _) = scanout
            .next_buffer()
            .context("failed to lease the initial GBM scanout buffer")?;
        let scanout_format = initial_scanout.format();
        if scanout_format.modifier == Modifier::Invalid
            || !gpu.renderable_scanout_formats.contains(&scanout_format)
        {
            bail!(
                "GBM selected scanout format {scanout_format:?}, which Weld cannot import through its explicit Vulkan modifier path"
            );
        }

        let imports = ScanoutImportCache::new(&gpu.device)?;
        let cursor_renderer = CursorOverlayRenderer::new(&gpu.device, &gpu.queue, SCANOUT_FORMAT);

        let (commands, command_receiver) = mpsc::channel();
        let epoch = Arc::new(AtomicU64::new(1));
        let active = Arc::new(AtomicBool::new(true));
        let worker_epoch = Arc::clone(&epoch);
        let worker_active = Arc::clone(&active);
        let worker_device = gpu.device.clone();

        let uncaptured_events = events.clone();
        gpu.device.on_uncaptured_error(Arc::new(move |error| {
            let _ = uncaptured_events.send(PresenterEvent::UncapturedError(error.to_string()));
        }));
        let device_lost_events = events.clone();
        gpu.device.set_device_lost_callback(move |reason, message| {
            let _ = device_lost_events
                .send(PresenterEvent::DeviceLost(format!("{reason:?}: {message}")));
        });

        let worker_events = events.clone();
        let worker = thread::Builder::new()
            .name("weld-drm-presenter".to_owned())
            .spawn(move || {
                let stopped_events = worker_events.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_worker(
                        worker_device,
                        command_receiver,
                        worker_events,
                        worker_epoch,
                        worker_active,
                    );
                }));
                if result.is_err() {
                    let _ = stopped_events.send(PresenterEvent::OutputUnavailable(
                        "GBM/KMS presenter worker panicked".to_owned(),
                    ));
                }
                let _ = stopped_events.send(PresenterEvent::Stopped);
            })
            .context("failed to spawn the GBM/KMS presenter worker")?;

        Ok(Self {
            commands,
            events,
            worker: Some(worker),
            epoch,
            active,
            crtc,
            scanout,
            frames: FrameTracker::new(),
            lifecycle: PresenterLifecycle::new(),
            device: gpu.device,
            queue: gpu.queue,
            imports,
            cursor_renderer,
            shutdown_requested: false,
        })
    }

    pub(super) fn target_availability(&self) -> PresenterTargetAvailability {
        if !self.active.load(Ordering::Acquire) {
            return PresenterTargetAvailability::Unavailable;
        }
        match self.lifecycle.state {
            PresenterState::Ready if self.frames.active.is_none() => {
                PresenterTargetAvailability::Ready
            }
            PresenterState::Starting
            | PresenterState::Ready
            | PresenterState::ActivatingAfterSession => PresenterTargetAvailability::Busy,
            PresenterState::Suspended | PresenterState::Unavailable | PresenterState::Stopped => {
                PresenterTargetAvailability::Unavailable
            }
        }
    }

    pub(super) fn acquire_frame(&mut self) -> Option<AcquiredFrame> {
        if self.target_availability() != PresenterTargetAvailability::Ready {
            return None;
        }
        match self.try_acquire_frame() {
            Ok(frame) => Some(frame),
            Err(error) => {
                self.report_unavailable(format!("failed to acquire direct GBM target: {error}"));
                None
            }
        }
    }

    fn try_acquire_frame(&mut self) -> Result<AcquiredFrame> {
        let ticket = self
            .frames
            .begin(PRESENTER_GENERATION, self.epoch.load(Ordering::Acquire))
            .context("physical frame became busy while acquiring its target")?;
        let acquired = (|| {
            let (scanout, _age) = self
                .scanout
                .next_buffer()
                .context("failed to lease GBM scanout buffer")?;
            let imported = self.imports.acquire(&self.device, &self.queue, &scanout)?;
            Ok(AcquiredFrame {
                ticket,
                target: CompositionTargetView::new(imported.view, imported.extent, SCANOUT_FORMAT),
                image: imported.image,
            })
        })();
        if acquired.is_err() {
            self.frames.release_rendering(ticket);
        }
        acquired
    }

    pub(super) fn finish_frame(
        &mut self,
        frame: &AcquiredFrame,
        cursor: &CursorOverlay,
    ) -> Result<()> {
        let release = barrier_command(
            &self.device,
            &self.imports.raw_device,
            self.imports.queue_family,
            frame.image,
            true,
            BarrierDirection::Release,
        )?;
        let mut cursor_encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("weld direct cursor encoder"),
                });
        self.cursor_renderer.encode(
            &mut cursor_encoder,
            frame.target.view(),
            frame.target.extent(),
            cursor,
        );
        let submission = self.queue.submit([cursor_encoder.finish(), release]);
        self.queue_completion(CompletionWork {
            ticket: frame.ticket,
            submission,
            present: true,
        });
        Ok(())
    }

    pub(super) fn abort_frame(&mut self, frame: AcquiredFrame) {
        let release = barrier_command(
            &self.device,
            &self.imports.raw_device,
            self.imports.queue_family,
            frame.image,
            true,
            BarrierDirection::Release,
        );
        match release {
            Ok(release) => {
                let submission = self.queue.submit([release]);
                self.queue_completion(CompletionWork {
                    ticket: frame.ticket,
                    submission,
                    present: false,
                });
            }
            Err(error) => {
                self.frames.release_rendering(frame.ticket);
                self.report_terminal_unavailable(format!(
                    "failed to release aborted GBM target: {error}"
                ));
            }
        }
    }

    fn queue_completion(&mut self, work: CompletionWork) {
        if let Err(mpsc::SendError(PresenterCommand::Wait(work))) =
            self.commands.send(PresenterCommand::Wait(work))
        {
            let outcome = wait_for_completion(&self.device, &work);
            let _ = self.events.send(PresenterEvent::Frame(PresenterFrameEvent {
                ticket: work.ticket,
                kind: outcome,
            }));
        }
    }

    pub(super) fn suspend(&mut self) {
        self.lifecycle.suspend();
        self.active.store(false, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.frames.suspend();
        let _ = self.commands.send(PresenterCommand::Suspend);
    }

    pub(super) fn activate_after_session(&mut self) -> Result<()> {
        self.active.store(true, Ordering::Release);
        if !self
            .lifecycle
            .begin_activation(self.frames.has_rendering_frame())
        {
            debug!("delaying GBM/KMS activation until the leased buffer is idle");
            return Ok(());
        }
        self.finish_activation()
    }

    fn finish_activation(&mut self) -> Result<()> {
        self.scanout
            .clear_pending_scanout()
            .inspect_err(|_| self.lifecycle.disable())
            .context("failed to clear stale GBM scanout state")?;
        let epoch = self.epoch.load(Ordering::Acquire);
        self.lifecycle.mark_ready();
        debug!(epoch, "GBM/KMS presenter activated");
        Ok(())
    }

    pub(super) fn handle_event(&mut self, event: &PresenterEvent) {
        match event {
            PresenterEvent::Ready { epoch }
                if *epoch == self.epoch.load(Ordering::Acquire)
                    && self.active.load(Ordering::Acquire) =>
            {
                self.lifecycle.mark_ready();
            }
            PresenterEvent::Frame(frame) => self.handle_frame_event(*frame),
            PresenterEvent::DeviceLost(_) => {
                self.lifecycle.disable();
                self.active.store(false, Ordering::Release);
                self.epoch.fetch_add(1, Ordering::AcqRel);
                self.frames.suspend();
            }
            PresenterEvent::Stopped => {
                self.lifecycle.stop();
                self.frames.clear();
            }
            PresenterEvent::OutputUnavailable(_)
            | PresenterEvent::UncapturedError(_)
            | PresenterEvent::Ready { .. } => {}
        }
    }

    pub(super) fn frame_submitted(&mut self, event_crtc: crtc::Handle) {
        if event_crtc != self.crtc {
            debug!(?event_crtc, expected = ?self.crtc, "ignored vblank for another CRTC");
            return;
        }
        match self.scanout.frame_submitted() {
            Ok(Some(ticket)) => {
                if !self.frames.release_vblank(ticket) {
                    debug!(?ticket, "ignored stale GBM/KMS vblank ticket");
                } else {
                    self.lifecycle.reset_recovery_budget();
                }
            }
            Ok(None) => debug!(?event_crtc, "ignored vblank without a pending Weld frame"),
            Err(error) => self.report_unavailable(format!(
                "failed to retire GBM/KMS frame after vblank: {error}"
            )),
        }
    }

    pub(super) const fn stopped(&self) -> bool {
        self.lifecycle.is_stopped()
    }

    pub(super) fn begin_shutdown(&mut self) {
        if self.shutdown_requested {
            return;
        }
        self.shutdown_requested = true;
        self.lifecycle.suspend();
        self.active.store(false, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.frames.clear();
        let _ = self.commands.send(PresenterCommand::Shutdown);
    }

    pub(super) fn join_if_finished(&mut self) {
        if (self.stopped() || self.worker.as_ref().is_some_and(JoinHandle::is_finished))
            && let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            warn!("GBM/KMS presenter worker panicked during shutdown");
        }
    }

    fn handle_frame_event(&mut self, event: PresenterFrameEvent) {
        if event.ticket.epoch != self.epoch.load(Ordering::Acquire) {
            if self.frames.release_rendering(event.ticket) {
                debug!(ticket = ?event.ticket, "retired GPU work from an older VT epoch");
                self.finish_deferred_activation();
            } else {
                debug!(ticket = ?event.ticket, "ignored presenter event from an older VT epoch");
            }
            return;
        }
        match event.kind {
            PresenterFrameEventKind::Prepared => {
                if !self.frames.is_rendering(event.ticket) {
                    debug!(ticket = ?event.ticket, "ignored stale prepared scanout buffer");
                    return;
                }
                match self.scanout.queue_buffer(None, None, event.ticket) {
                    Ok(()) => {
                        if !self.frames.mark_awaiting_vblank(event.ticket) {
                            self.report_unavailable(
                                "prepared GBM frame lost its rendering state".to_owned(),
                            );
                        }
                    }
                    Err(GbmBufferedSurfaceError::DrmError(DrmError::DeviceInactive)) => {
                        self.frames.release_rendering(event.ticket);
                    }
                    Err(error) => {
                        self.frames.release_rendering(event.ticket);
                        self.report_unavailable(format!(
                            "failed to queue completed GBM buffer: {error}"
                        ));
                    }
                }
            }
            PresenterFrameEventKind::Released(outcome) => {
                let released = self.frames.release_rendering(event.ticket);
                if !released {
                    debug!(ticket = ?event.ticket, ?outcome, "ignored stale worker release");
                    return;
                }
                if outcome == FrameOutcome::Unavailable {
                    self.report_unavailable("GPU scanout preparation failed".to_owned());
                }
                self.finish_deferred_activation();
            }
        }
    }

    fn finish_deferred_activation(&mut self) {
        if self.lifecycle.worker_became_idle()
            && let Err(error) = self.finish_activation()
        {
            self.report_terminal_unavailable(format!(
                "failed to finish deferred GBM/KMS activation: {error}"
            ));
        }
    }

    fn report_unavailable(&mut self, message: String) {
        let _ = self.events.send(PresenterEvent::OutputUnavailable(message));
        self.frames.drop_awaiting_vblank();
        if !self.active.load(Ordering::Acquire)
            || self.frames.has_rendering_frame()
            || !self.lifecycle.begin_transient_recovery()
        {
            return;
        }
        match self.scanout.clear_pending_scanout() {
            Ok(()) => {
                self.lifecycle.mark_ready();
            }
            Err(error) => self.report_terminal_unavailable(format!(
                "failed to reset GBM/KMS after a presentation error: {error}"
            )),
        }
    }

    fn report_terminal_unavailable(&mut self, message: String) {
        self.lifecycle.disable();
        let _ = self.events.send(PresenterEvent::OutputUnavailable(message));
    }
}

impl Drop for PresenterHandle {
    fn drop(&mut self) {
        self.begin_shutdown();
        self.join_if_finished();
    }
}

fn run_worker(
    device: wgpu::Device,
    commands: mpsc::Receiver<PresenterCommand>,
    events: CalloopSender<PresenterEvent>,
    epoch: Arc<AtomicU64>,
    active: Arc<AtomicBool>,
) {
    #[cfg(feature = "profiling-tracy")]
    if let Some(client) = tracing_tracy::client::Client::running() {
        client.set_thread_name("weld-drm-presenter");
    }

    let _ = events.send(PresenterEvent::Ready {
        epoch: epoch.load(Ordering::Acquire),
    });

    while let Ok(command) = commands.recv() {
        match command {
            PresenterCommand::Wait(work) => {
                let _span = tracing::trace_span!(
                    target: crate::PROFILE_TARGET,
                    "drm_wait_for_frame"
                )
                .entered();
                let mut kind = wait_for_completion(&device, &work);
                if matches!(kind, PresenterFrameEventKind::Prepared)
                    && (!active.load(Ordering::Acquire)
                        || epoch.load(Ordering::Acquire) != work.ticket.epoch)
                {
                    kind = PresenterFrameEventKind::Released(FrameOutcome::Interrupted);
                }
                let _ = events.send(PresenterEvent::Frame(PresenterFrameEvent {
                    ticket: work.ticket,
                    kind,
                }));
            }
            PresenterCommand::Suspend => {}
            PresenterCommand::Shutdown => break,
        }
    }
}

fn wait_for_completion(device: &wgpu::Device, work: &CompletionWork) -> PresenterFrameEventKind {
    match device.poll(wgpu::PollType::Wait {
        submission_index: Some(work.submission.clone()),
        timeout: None,
    }) {
        Ok(_) if work.present => PresenterFrameEventKind::Prepared,
        Ok(_) => PresenterFrameEventKind::Released(FrameOutcome::Interrupted),
        Err(error) => {
            warn!(%error, "failed to complete direct GBM frame");
            PresenterFrameEventKind::Released(FrameOutcome::Unavailable)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ScanoutAllocationKey {
    device: u64,
    inode: u64,
    width: i32,
    height: i32,
    format: u32,
    modifier: u64,
    stride: u32,
    offset: u32,
}

struct ImportedScanout {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    image: vk::Image,
    used: bool,
}

struct ScanoutImportCache {
    raw_device: ash::Device,
    queue_family: u32,
    imports: HashMap<ScanoutAllocationKey, ImportedScanout>,
}

impl ScanoutImportCache {
    fn new(device: &wgpu::Device) -> Result<Self> {
        // SAFETY: the copied Vulkan device handle and queue-family index remain
        // valid because the owning wgpu device outlives this worker cache.
        let (raw_device, queue_family) = unsafe {
            let raw = device
                .as_hal::<wgpu::hal::api::Vulkan>()
                .context("scanout device is not backed by Vulkan")?;
            (raw.raw_device().clone(), raw.queue_family_index())
        };
        Ok(Self {
            raw_device,
            queue_family,
            imports: HashMap::new(),
        })
    }

    fn acquire(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        dmabuf: &Dmabuf,
    ) -> Result<ImportedScanoutTarget> {
        let raw_device = self.raw_device.clone();
        let queue_family = self.queue_family;
        let target = self.import(device, dmabuf)?;
        let acquire = barrier_command(
            device,
            &raw_device,
            queue_family,
            target.image,
            target.used,
            BarrierDirection::Acquire,
        )?;

        queue.submit([acquire]);
        target.used = true;
        let size = dmabuf.size();
        Ok(ImportedScanoutTarget {
            view: target.view.clone(),
            image: target.image,
            extent: Extent::new(
                u32::try_from(size.w).context("negative GBM buffer width")?,
                u32::try_from(size.h).context("negative GBM buffer height")?,
            ),
        })
    }

    fn import<'a>(
        &'a mut self,
        device: &wgpu::Device,
        dmabuf: &Dmabuf,
    ) -> Result<&'a mut ImportedScanout> {
        let key = scanout_key(dmabuf)?;
        match self.imports.entry(key) {
            Entry::Occupied(imported) => Ok(imported.into_mut()),
            Entry::Vacant(entry) => {
                let imported = import_scanout(device, dmabuf)?;
                Ok(entry.insert(imported))
            }
        }
    }
}

struct ImportedScanoutTarget {
    view: wgpu::TextureView,
    image: vk::Image,
    extent: Extent,
}

#[derive(Clone, Copy)]
enum BarrierDirection {
    Acquire,
    Release,
}

fn barrier_command(
    device: &wgpu::Device,
    raw_device: &ash::Device,
    queue_family: u32,
    image: vk::Image,
    previously_used: bool,
    direction: BarrierDirection,
) -> Result<wgpu::CommandBuffer> {
    let acquire = matches!(direction, BarrierDirection::Acquire);
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some(if acquire {
            "weld GBM foreign acquire"
        } else {
            "weld GBM foreign release"
        }),
    });
    // SAFETY: this encoder contains only raw Vulkan commands. It is never used
    // through the ordinary wgpu encoding API, the imported image is retained
    // through submission completion, and the adjacent batch command buffers
    // establish the declared acquire-render-release order.
    let recorded = unsafe {
        encoder.as_hal_mut::<wgpu::hal::api::Vulkan, _, _>(|raw_encoder| {
            let raw_encoder = raw_encoder?;
            let first_acquire = acquire && !previously_used;
            let barrier = vk::ImageMemoryBarrier::default()
                .src_access_mask(if acquire {
                    if first_acquire {
                        vk::AccessFlags::empty()
                    } else {
                        vk::AccessFlags::MEMORY_READ
                    }
                } else {
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE
                })
                .dst_access_mask(if acquire {
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE
                } else {
                    vk::AccessFlags::MEMORY_READ
                })
                .old_layout(if first_acquire {
                    vk::ImageLayout::UNDEFINED
                } else if acquire {
                    vk::ImageLayout::GENERAL
                } else {
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
                })
                .new_layout(if acquire {
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
                } else {
                    vk::ImageLayout::GENERAL
                })
                .src_queue_family_index(if first_acquire {
                    vk::QUEUE_FAMILY_IGNORED
                } else if acquire {
                    vk::QUEUE_FAMILY_FOREIGN_EXT
                } else {
                    queue_family
                })
                .dst_queue_family_index(if first_acquire {
                    vk::QUEUE_FAMILY_IGNORED
                } else if acquire {
                    queue_family
                } else {
                    vk::QUEUE_FAMILY_FOREIGN_EXT
                })
                .image(image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                });
            raw_device.cmd_pipeline_barrier(
                raw_encoder.raw_handle(),
                if acquire {
                    vk::PipelineStageFlags::TOP_OF_PIPE
                } else {
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                },
                if acquire {
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                } else {
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE
                },
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
            Some(())
        })
    };
    recorded.context("wgpu did not expose a Vulkan encoder for scanout ownership")?;
    Ok(encoder.finish())
}

fn scanout_key(dmabuf: &Dmabuf) -> Result<ScanoutAllocationKey> {
    if dmabuf.num_planes() != 1 {
        bail!("GBM scanout import requires exactly one DMA-BUF plane");
    }
    let handle = dmabuf.handles().next().context("GBM buffer has no plane")?;
    let stat = fstat(handle).context("failed to identify GBM allocation")?;
    let size = dmabuf.size();
    Ok(ScanoutAllocationKey {
        device: stat.st_dev,
        inode: stat.st_ino,
        width: size.w,
        height: size.h,
        format: dmabuf.format().code as u32,
        modifier: dmabuf.format().modifier.into(),
        stride: dmabuf
            .strides()
            .next()
            .context("GBM buffer has no stride")?,
        offset: dmabuf
            .offsets()
            .next()
            .context("GBM buffer has no offset")?,
    })
}

fn import_scanout(device: &wgpu::Device, dmabuf: &Dmabuf) -> Result<ImportedScanout> {
    let size = dmabuf.size();
    let width = u32::try_from(size.w).context("negative GBM buffer width")?;
    let height = u32::try_from(size.h).context("negative GBM buffer height")?;
    let handle = dmabuf.handles().next().context("GBM buffer has no plane")?;
    let fd = handle
        .try_clone_to_owned()
        .context("failed to duplicate GBM buffer fd")?;
    let stride = u64::from(
        dmabuf
            .strides()
            .next()
            .context("GBM buffer has no stride")?,
    );
    let offset = u64::from(
        dmabuf
            .offsets()
            .next()
            .context("GBM buffer has no offset")?,
    );
    let extent = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let hal_descriptor = wgpu::hal::TextureDescriptor {
        label: Some("weld imported GBM scanout"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: SCANOUT_FORMAT,
        usage: wgpu::TextureUses::COLOR_TARGET,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: Vec::new(),
    };
    // SAFETY: GbmBufferedSurface chose this exact single-plane modifier from
    // the intersection of KMS and Vulkan color-attachment capabilities. The
    // duplicated fd and plane layout describe that allocation, and Vulkan
    // consumes only the duplicate.
    let hal_texture = unsafe {
        let raw = device
            .as_hal::<wgpu::hal::api::Vulkan>()
            .context("scanout device is not backed by Vulkan")?;
        raw.texture_from_dmabuf_fd(
            fd,
            &hal_descriptor,
            dmabuf.format().modifier.into(),
            stride,
            offset,
        )?
    };
    let descriptor = wgpu::TextureDescriptor {
        label: Some("weld imported GBM scanout"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: SCANOUT_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    };
    // SAFETY: the HAL texture was created by this exact device and descriptor.
    // The tracker remains in COLOR_TARGET while explicit raw barriers transfer
    // actual ownership to and from KMS around each ordinary wgpu render pass.
    let texture = unsafe {
        device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(
            hal_texture,
            &descriptor,
            wgpu::TextureUses::COLOR_TARGET,
        )
    };
    // SAFETY: the HAL guard remains live while its opaque image handle is
    // copied, and the cache retains the wgpu texture through every submission.
    let image = unsafe {
        texture
            .as_hal::<wgpu::hal::api::Vulkan>()
            .context("imported scanout texture is not backed by Vulkan")?
            .raw_handle()
    };
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Ok(ImportedScanout {
        _texture: texture,
        view,
        image,
        used: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames() -> FrameTracker {
        FrameTracker::new()
    }

    #[test]
    fn activation_waits_for_the_worker_before_rearming_scanout() {
        let mut lifecycle = PresenterLifecycle::new();
        lifecycle.mark_ready();
        lifecycle.suspend();

        assert!(!lifecycle.begin_activation(true));
        assert_eq!(lifecycle.state, PresenterState::ActivatingAfterSession);
        assert!(lifecycle.worker_became_idle());

        lifecycle.mark_ready();
        assert_eq!(lifecycle.state, PresenterState::Ready);
    }

    #[test]
    fn transient_recovery_stops_after_its_bounded_attempts() {
        let mut lifecycle = PresenterLifecycle::new();
        lifecycle.mark_ready();

        for _ in 0..TRANSIENT_RECOVERY_ATTEMPTS {
            assert!(lifecycle.begin_transient_recovery());
            lifecycle.mark_ready();
        }
        assert!(!lifecycle.begin_transient_recovery());
        assert_eq!(lifecycle.state, PresenterState::Unavailable);

        assert!(lifecycle.begin_activation(false));
        lifecycle.mark_ready();
        assert!(lifecycle.begin_transient_recovery());
    }

    #[test]
    fn a_second_frame_cannot_start_until_the_active_frame_retires() {
        let mut frames = frames();
        let first = frames.begin(1, 1).expect("first frame should start");

        assert!(frames.begin(1, 1).is_none());
        assert!(frames.mark_awaiting_vblank(first));
        assert!(frames.release_vblank(first));
        assert!(frames.begin(1, 1).is_some());
    }

    #[test]
    fn wrong_phase_cannot_release_active_frame() {
        let mut frames = frames();
        let frame = frames.begin(1, 1).expect("frame should start");

        assert!(!frames.release_vblank(frame));
        assert!(frames.release_rendering(frame));
    }

    #[test]
    fn suspend_keeps_a_worker_owned_target_until_rendering_finishes() {
        let mut frames = frames();
        let rendering = frames.begin(1, 1).expect("frame should start");

        frames.suspend();

        assert!(frames.is_rendering(rendering));
        assert!(frames.release_rendering(rendering));

        let presented = frames.begin(1, 2).expect("frame should restart");
        assert!(frames.mark_awaiting_vblank(presented));
        frames.suspend();
        assert!(frames.active.is_none());
    }

    #[test]
    fn stale_worker_or_vblank_ticket_cannot_release_current_frame() {
        let mut frames = frames();
        let old = frames.begin(1, 4).expect("old frame should start");
        frames.clear();
        let current = frames.begin(1, 6).expect("current frame should start");

        assert!(!frames.release_rendering(old));
        assert!(!frames.release_vblank(old));
        assert!(frames.mark_awaiting_vblank(current));
        assert!(frames.release_vblank(current));
    }
}
