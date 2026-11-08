//! Output and window image-copy sessions, bounded GPU work, and SHM delivery.

use calloop::{Interest, LoopHandle, RegistrationToken};
use std::{
    cell::Cell,
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use smithay::{
    backend::{
        allocator::{Buffer, Fourcc},
        drm::DrmNode,
    },
    output::Output,
    reexports::wayland_server::{DisplayHandle, Resource, protocol::wl_shm},
    utils::{Clock, IsAlive, Monotonic, Rectangle, Transform},
    wayland::{
        dmabuf::get_dmabuf,
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
            OutputCaptureSourceState,
        },
        image_copy_capture::{
            BufferConstraints, CaptureFailureReason, DmabufConstraints, Frame, FrameRef,
            ImageCopyCaptureHandler, ImageCopyCaptureState, Session, SessionRef,
        },
        shm::{BufferData, with_buffer_contents, with_buffer_contents_mut},
    },
};
use tracing::warn;

use super::ServerState;
use crate::{
    OutputId,
    dmabuf::DmabufCapabilities,
    host::CompositionFrame,
    renderer::{
        capture_gpu::{GpuCapture, GpuCaptureTask},
        capture_snapshot::snapshot_composition,
        capture_task::{CaptureTask, CaptureWorker},
        read_owned_frame_async,
    },
    surface::{Extent, SurfaceId},
};

mod cursor;
mod window;
use cursor::CaptureCursor;

const MAX_SESSIONS: usize = 64;
const MAX_FRAMES: usize = 8;
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureTarget {
    Output(OutputId),
    Window(SurfaceId),
}
impl CaptureTarget {
    fn of(source: &ImageCaptureSource) -> Option<Self> {
        source
            .user_data()
            .get::<OutputId>()
            .copied()
            .map(Self::Output)
            .or_else(|| {
                source
                    .user_data()
                    .get::<SurfaceId>()
                    .copied()
                    .map(Self::Window)
            })
    }
}

pub(super) struct ImageCaptures {
    sources: OutputCaptureSourceState,
    windows: window::WindowSources,
    copies: ImageCopyCaptureState,
    enabled: bool,
    sessions: Vec<Session>,
    pending: Vec<PendingFrame>,
    dma: Option<DmabufConstraints>,
    gpu: Option<GpuCapture>,
    cached: HashMap<OutputId, CompositionFrame>,
    loop_handle: LoopHandle<'static, ServerState>,
}

struct PendingFrame {
    target: CaptureTarget,
    session: SessionRef,
    frame: Frame,
    extent: Extent,
    deadline: Instant,
    work: FrameWork,
    sampled_at: Duration,
    cursor: Option<CaptureCursor>,
    writable: Option<BufferReady>,
}

struct BufferReady {
    ready: Rc<Cell<bool>>,
    token: RegistrationToken,
    handle: LoopHandle<'static, ServerState>,
}
impl Drop for BufferReady {
    fn drop(&mut self) {
        self.handle.remove(self.token);
    }
}

enum FrameWork {
    Waiting,
    Readback(CaptureTask),
    Gpu(GpuCaptureTask),
    Failed(CaptureFailureReason),
}

impl ImageCaptures {
    pub(super) fn new(
        display: &DisplayHandle,
        loop_handle: LoopHandle<'static, ServerState>,
        capabilities: Option<&DmabufCapabilities>,
    ) -> Self {
        let dma = capabilities.and_then(|cap| {
            if cap.capture_formats.is_empty() {
                return None;
            }
            Some(DmabufConstraints {
                node: DrmNode::from_dev_id(cap.main_device).ok()?,
                formats: [Fourcc::Argb8888, Fourcc::Xrgb8888]
                    .into_iter()
                    .filter_map(|code| {
                        let modifiers: Vec<_> = cap
                            .capture_formats
                            .iter()
                            .filter(|format| format.code == code)
                            .map(|format| format.modifier)
                            .collect();
                        (!modifiers.is_empty()).then_some((code, modifiers))
                    })
                    .collect(),
            })
        });
        Self {
            sources: OutputCaptureSourceState::new::<ServerState>(display),
            windows: window::WindowSources::new(display, capabilities),
            copies: ImageCopyCaptureState::new::<ServerState>(display),
            enabled: false,
            sessions: Vec::new(),
            pending: Vec::new(),
            dma,
            gpu: None,
            cached: HashMap::new(),
            loop_handle,
        }
    }
}

impl ImageCaptureSourceHandler for ServerState {}

impl OutputCaptureSourceHandler for ServerState {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.capture.sources
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        if let Some((&id, _)) = self.outputs.iter().find(|(_, item)| item.native == *output) {
            source.user_data().insert_if_missing(|| id);
        }
    }
}

impl ImageCopyCaptureHandler for ServerState {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.capture.copies
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        if !self.capture.enabled || self.session_locked() {
            return None;
        }
        let size = match CaptureTarget::of(source)? {
            CaptureTarget::Output(id) => {
                let size = self.outputs.get(&id)?.metrics.mode().size;
                (size.w, size.h).into()
            }
            CaptureTarget::Window(id) => {
                if source
                    .user_data()
                    .get::<smithay::wayland::foreign_toplevel_list::ForeignToplevelHandle>()
                    .is_none_or(|handle| handle.is_closed())
                {
                    return None;
                }
                let extent = self.capture.windows.extent(id)?;
                (
                    i32::try_from(extent.width).ok()?,
                    i32::try_from(extent.height).ok()?,
                )
                    .into()
            }
        };
        Some(BufferConstraints {
            size,
            shm: vec![wl_shm::Format::Xrgb8888],
            dma: self.capture.dma.clone(),
        })
    }

    fn new_session(&mut self, session: Session) {
        if self.capture.sessions.len() < MAX_SESSIONS {
            self.capture.sessions.push(session);
            self.refresh_capture_demand();
        } else {
            session.stop();
        }
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        let source = session.source();
        let Some(constraints) = self.capture_constraints(&source) else {
            frame.fail(CaptureFailureReason::Stopped);
            return;
        };
        let Some(target) = CaptureTarget::of(&source) else {
            frame.fail(CaptureFailureReason::Stopped);
            return;
        };
        let buffer_matches = if let Ok(dmabuf) = get_dmabuf(&frame.buffer()) {
            dmabuf.size() == constraints.size
                && dmabuf.num_planes() == 1
                && dmabuf.flags().is_empty()
                && constraints.dma.as_ref().is_some_and(|dma| {
                    dma.formats.iter().any(|(code, modifiers)| {
                        *code == dmabuf.format().code
                            && modifiers.contains(&dmabuf.format().modifier)
                    })
                })
        } else {
            with_buffer_contents(&frame.buffer(), |_, _, data| {
                (data.width, data.height) == (constraints.size.w, constraints.size.h)
            })
            .unwrap_or(false)
        };
        if !buffer_matches
            || session
                .current_constraints()
                .is_none_or(|current| current.size != constraints.size)
        {
            session.update_constraints(constraints);
            frame.fail(CaptureFailureReason::BufferConstraints);
            return;
        }
        if self.capture.pending.len() >= MAX_FRAMES {
            frame.fail(CaptureFailureReason::Unknown);
            return;
        }
        let (Ok(width), Ok(height)) = (
            u32::try_from(constraints.size.w),
            u32::try_from(constraints.size.h),
        ) else {
            frame.fail(CaptureFailureReason::BufferConstraints);
            return;
        };
        let writable = if let Ok(buffer) = get_dmabuf(&frame.buffer())
            && let Ok((_, source)) = buffer.generate_blocker(Interest::WRITE)
        {
            let ready = Rc::new(Cell::new(false));
            let signal = ready.clone();
            match self
                .capture
                .loop_handle
                .insert_source(source, move |_, _, _| {
                    signal.set(true);
                    Ok(())
                }) {
                Ok(token) => Some(BufferReady {
                    ready,
                    token,
                    handle: self.capture.loop_handle.clone(),
                }),
                Err(error) => {
                    warn!(%error, "could not watch capture buffer fences");
                    frame.fail(CaptureFailureReason::Unknown);
                    return;
                }
            }
        } else {
            None
        };
        self.capture.pending.push(PendingFrame {
            target,
            session: session.clone(),
            frame,
            extent: Extent::new(width, height),
            deadline: Instant::now() + FRAME_TIMEOUT,
            work: FrameWork::Waiting,
            sampled_at: Duration::ZERO,
            cursor: None,
            writable,
        });
        self.refresh_capture_demand();
    }

    fn frame_aborted(&mut self, frame: FrameRef) {
        self.capture
            .pending
            .retain(|pending| pending.frame != frame);
        self.refresh_capture_demand();
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.capture
            .pending
            .retain(|pending| pending.session != session);
        self.capture.sessions.retain(|owned| *owned != session);
        self.capture.copies.cleanup();
        self.refresh_capture_demand();
    }
}

impl ServerState {
    pub(super) fn stop_captures(&mut self) {
        for pending in self.capture.pending.drain(..) {
            pending.frame.fail(CaptureFailureReason::Stopped);
        }
        for session in self.capture.sessions.drain(..) {
            session.stop();
        }
        self.capture.cached.clear();
        self.refresh_capture_demand();
    }
    pub(crate) fn enable_capture(&mut self) {
        self.capture.enabled = true;
    }

    pub(crate) fn output_capture_requested(&self) -> bool {
        self.capture.pending.iter().any(|pending| {
            matches!(pending.target, CaptureTarget::Output(_))
                && matches!(pending.work, FrameWork::Waiting)
        })
    }

    /// Only the first request without a current snapshot needs composition.
    pub(crate) fn output_capture_needs_composition(&self) -> bool {
        self.capture.pending.iter().any(|pending| {
            let CaptureTarget::Output(output) = pending.target else {
                return false;
            };
            matches!(pending.work, FrameWork::Waiting)
                && self
                    .capture
                    .cached
                    .get(&output)
                    .is_none_or(|frame| frame.target().extent() != pending.extent)
        })
    }

    pub(crate) fn capture_cached_outputs(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        worker: &CaptureWorker,
    ) {
        let cached = std::mem::take(&mut self.capture.cached);
        for (output, frame) in &cached {
            self.deliver_output_frame(*output, frame, device, queue, worker);
        }
        self.capture.cached = cached;
    }

    pub(crate) fn capture_timeout(&self, now: Instant) -> Option<Duration> {
        self.capture
            .pending
            .iter()
            .filter_map(|pending| match pending.work {
                FrameWork::Failed(_) => Some(Duration::ZERO),
                // Submitted writes retain their allocations until completion;
                // an expired CPU deadline must not turn a slow GPU into polling.
                FrameWork::Gpu(_) => None,
                _ => Some(pending.deadline.saturating_duration_since(now)),
            })
            .min()
    }

    /// Submit a copy before the presenter can reuse its composition texture.
    pub(crate) fn capture_output_frame(
        &mut self,
        output: OutputId,
        frame: &CompositionFrame,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        worker: &CaptureWorker,
    ) {
        if !self
            .capture
            .sessions
            .iter()
            .any(|session| session.source().user_data().get::<OutputId>() == Some(&output))
        {
            return;
        }
        let snapshot =
            snapshot_composition(device, queue, frame, self.capture.cached.remove(&output));
        match snapshot {
            Ok(snapshot) => {
                self.deliver_output_frame(output, &snapshot, device, queue, worker);
                self.capture.cached.insert(output, snapshot);
            }
            Err(error) => {
                warn!(%error, "could not retain output for capture");
            }
        }
    }

    fn deliver_output_frame(
        &mut self,
        output: OutputId,
        frame: &CompositionFrame,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        worker: &CaptureWorker,
    ) {
        if self.session_locked() {
            return;
        }
        let sampled_at = Clock::<Monotonic>::new().now().into();
        let draw_cursor = self.capture.pending.iter().any(|pending| {
            pending.target == CaptureTarget::Output(output)
                && matches!(pending.work, FrameWork::Waiting)
                && pending.session.draw_cursor()
        });
        let cursor = if draw_cursor {
            self.capture_cursor(output)
        } else {
            Ok(None)
        };
        for pending in &mut self.capture.pending {
            if pending.target != CaptureTarget::Output(output)
                || !matches!(pending.work, FrameWork::Waiting)
                || pending.frame.has_failed()
                || pending
                    .writable
                    .as_ref()
                    .is_some_and(|fence| !fence.ready.get())
            {
                continue;
            }
            if frame.target().extent() != pending.extent {
                // A new request will use the session's updated output constraints.
                pending.work = FrameWork::Failed(CaptureFailureReason::BufferConstraints);
                continue;
            }
            if pending.session.draw_cursor() {
                match &cursor {
                    Ok(cursor) => pending.cursor = cursor.clone(),
                    Err(error) => {
                        warn!(%error, "could not capture output cursor");
                        pending.work = FrameWork::Failed(CaptureFailureReason::Unknown);
                        continue;
                    }
                }
            }
            pending.writable = None;
            if let Ok(buffer) = get_dmabuf(&pending.frame.buffer()) {
                let result = (|| -> Result<GpuCaptureTask> {
                    if self.capture.gpu.is_none() {
                        self.capture.gpu = Some(GpuCapture::new(device, queue, worker.notifier())?);
                    }
                    let gpu = self
                        .capture
                        .gpu
                        .as_mut()
                        .context("GPU capture unavailable")?;
                    gpu.submit(buffer, frame.target().view(), |blitter, target, encoder| {
                        if let Some(cursor) = &pending.cursor {
                            cursor.paint_gpu(device, queue, blitter, target, encoder)?;
                        }
                        Ok(())
                    })
                })();
                match result {
                    Ok(task) => {
                        pending.work = FrameWork::Gpu(task);
                        pending.sampled_at = sampled_at;
                        tracing::trace!(target: "weld_capture", ?output, "submitted DMA-BUF output capture");
                    }
                    Err(error) => {
                        warn!(%error, "could not submit DMA-BUF output capture");
                        pending.work = FrameWork::Failed(CaptureFailureReason::Unknown);
                    }
                }
                continue;
            }
            match read_owned_frame_async(device, queue, frame, None, worker) {
                Ok(task) => {
                    pending.work = FrameWork::Readback(task);
                    pending.sampled_at = sampled_at;
                }
                Err(error) => {
                    warn!(%error, "could not submit output capture");
                    pending.work = FrameWork::Failed(CaptureFailureReason::Unknown);
                }
            }
        }
    }

    pub(crate) fn poll_captures(&mut self) {
        let now = Instant::now();
        let mut index = 0;
        while index < self.capture.pending.len() {
            let pending = &self.capture.pending[index];
            if !pending.session.alive()
                || pending.frame.has_failed()
                || !pending.frame.buffer().is_alive()
            {
                self.capture
                    .pending
                    .swap_remove(index)
                    .frame
                    .fail(CaptureFailureReason::Stopped);
                continue;
            }
            if let FrameWork::Failed(reason) = pending.work {
                self.capture.pending.swap_remove(index).frame.fail(reason);
                continue;
            }
            if let FrameWork::Gpu(task) = &pending.work {
                if let Some(result) = task.poll() {
                    let pending = self.capture.pending.swap_remove(index);
                    match result {
                        Ok(()) => pending.frame.success(
                            Transform::Normal,
                            Some(vec![Rectangle::from_size(
                                (pending.extent.width as i32, pending.extent.height as i32).into(),
                            )]),
                            pending.sampled_at,
                        ),
                        Err(error) => {
                            warn!(%error, "GPU capture completion failed");
                            pending.frame.fail(CaptureFailureReason::Unknown);
                        }
                    }
                } else {
                    index += 1;
                }
                continue;
            }
            let result = match &pending.work {
                FrameWork::Readback(task) => task.poll(),
                _ => None,
            };
            if let Some(result) = result {
                let pending = self.capture.pending.swap_remove(index);
                let result = result.map_err(anyhow::Error::msg).and_then(|mut pixels| {
                    if let Some(cursor) = &pending.cursor {
                        cursor.paint(pending.extent, &mut pixels)?;
                    }
                    with_buffer_contents_mut(&pending.frame.buffer(), |pointer, length, data| {
                        write_pixels(pointer, length, data, pending.extent, &mut pixels)
                    })
                    .context("capture buffer is not writable SHM")?
                });
                match result {
                    Ok(()) => pending.frame.success(
                        Transform::Normal,
                        Some(vec![Rectangle::from_size(
                            (pending.extent.width as i32, pending.extent.height as i32).into(),
                        )]),
                        pending.sampled_at,
                    ),
                    Err(error) => {
                        warn!(%error, "could not deliver output capture");
                        pending.frame.fail(CaptureFailureReason::Unknown);
                    }
                }
            } else if pending.deadline <= now {
                self.capture
                    .pending
                    .swap_remove(index)
                    .frame
                    .fail(CaptureFailureReason::Unknown);
            } else {
                index += 1;
            }
        }
        self.capture.sessions.retain(|session| session.alive());
        self.capture.cached.retain(|id, _| {
            self.capture
                .sessions
                .iter()
                .any(|session| session.source().user_data().get::<OutputId>() == Some(id))
        });
        if let Some(gpu) = &mut self.capture.gpu {
            gpu.prune();
        }
        self.capture.copies.cleanup();
        let sessions: Vec<_> = self.capture.sessions.iter().map(Session::as_ref).collect();
        for session in sessions {
            if let Some(constraints) = self.capture_constraints(&session.source()) {
                if session
                    .current_constraints()
                    .is_none_or(|old| old.size != constraints.size)
                {
                    session.update_constraints(constraints);
                }
            } else if let Some(index) = self
                .capture
                .sessions
                .iter()
                .position(|owned| *owned == session)
            {
                self.capture.sessions.swap_remove(index).stop();
            }
        }
        self.refresh_capture_demand();
    }
}

fn write_pixels(
    pointer: *mut u8,
    pool_length: usize,
    data: BufferData,
    extent: Extent,
    pixels: &mut [u8],
) -> Result<()> {
    let width = usize::try_from(data.width)?;
    let height = usize::try_from(data.height)?;
    ensure!(
        width == usize::try_from(extent.width)? && height == usize::try_from(extent.height)?,
        "capture buffer size changed"
    );
    ensure!(
        data.format == wl_shm::Format::Xrgb8888,
        "unsupported capture format"
    );
    let stride = usize::try_from(data.stride)?;
    let offset = usize::try_from(data.offset)?;
    let row_bytes = width.checked_mul(4).context("capture row size overflow")?;
    let total = row_bytes
        .checked_mul(height)
        .context("capture size overflow")?;
    ensure!(
        height > 0 && row_bytes > 0 && stride >= row_bytes && pixels.len() == total,
        "invalid capture row layout"
    );
    let end = stride
        .checked_mul(height - 1)
        .and_then(|span| span.checked_add(row_bytes))
        .and_then(|span| offset.checked_add(span))
        .context("capture pool offset overflow")?;
    ensure!(end <= pool_length, "capture buffer extends beyond its pool");
    for pixel in pixels.as_chunks_mut::<4>().0 {
        *pixel = u32::from_be_bytes([255, pixel[0], pixel[1], pixel[2]]).to_ne_bytes();
    }
    for (row, source) in pixels.chunks_exact(row_bytes).enumerate() {
        // SAFETY: The checked row ranges fit Smithay's mapped pool for the duration
        // of this callback. The source is owned readback memory, disjoint from SHM.
        // Raw copying avoids creating references into client-controlled memory.
        unsafe {
            std::ptr::copy_nonoverlapping(
                source.as_ptr(),
                pointer.add(offset + row * stride),
                row_bytes,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> BufferData {
        BufferData {
            width: 1,
            height: 2,
            stride: 8,
            offset: 4,
            format: wl_shm::Format::Xrgb8888,
        }
    }

    #[test]
    fn capture_delivery_preserves_pool_padding_and_converts_rgba_to_native_xrgb() {
        let mut pool = [0x77; 24];
        let mut pixels = [10, 20, 30, 40, 50, 60, 70, 80];
        write_pixels(
            pool.as_mut_ptr(),
            pool.len(),
            data(),
            Extent::new(1, 2),
            &mut pixels,
        )
        .expect("valid capture");
        assert_eq!(&pool[4..8], &0xff0a141e_u32.to_ne_bytes());
        assert_eq!(&pool[12..16], &0xff323c46_u32.to_ne_bytes());
        for index in (0..4).chain(8..12).chain(16..24) {
            assert_eq!(pool[index], 0x77);
        }
    }

    #[test]
    fn invalid_capture_ranges_leave_client_memory_untouched() {
        for (layout, length, extent) in [
            (data(), 15, Extent::new(1, 2)),
            (
                BufferData {
                    stride: 3,
                    ..data()
                },
                24,
                Extent::new(1, 2),
            ),
            (
                BufferData {
                    offset: -1,
                    ..data()
                },
                24,
                Extent::new(1, 2),
            ),
            (data(), 24, Extent::new(2, 2)),
        ] {
            let mut pool = [0x77; 24];
            let mut pixels = [10; 8];
            assert!(write_pixels(pool.as_mut_ptr(), length, layout, extent, &mut pixels).is_err());
            assert_eq!(pool, [0x77; 24]);
        }
    }
}
