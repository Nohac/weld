//! Output capture sessions, bounded readback requests, and client SHM delivery.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use smithay::{
    output::Output,
    reexports::wayland_server::{DisplayHandle, Resource, protocol::wl_shm},
    utils::{Clock, IsAlive, Monotonic, Rectangle, Transform},
    wayland::{
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
            OutputCaptureSourceState,
        },
        image_copy_capture::{
            BufferConstraints, CaptureFailureReason, Frame, FrameRef, ImageCopyCaptureHandler,
            ImageCopyCaptureState, Session, SessionRef,
        },
        shm::{BufferData, with_buffer_contents, with_buffer_contents_mut},
    },
};
use tracing::warn;

use super::ServerState;
use crate::{
    OutputId,
    host::CompositionFrame,
    renderer::{
        capture_task::{CaptureTask, CaptureWorker},
        read_owned_frame_async,
    },
    surface::Extent,
};

mod cursor;
use cursor::CaptureCursor;

const MAX_SESSIONS: usize = 64;
const MAX_FRAMES: usize = 8;
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) struct OutputCapture {
    sources: OutputCaptureSourceState,
    copies: ImageCopyCaptureState,
    enabled: bool,
    sessions: Vec<Session>,
    pending: Vec<PendingFrame>,
}

struct PendingFrame {
    output: OutputId,
    session: SessionRef,
    frame: Frame,
    extent: Extent,
    deadline: Instant,
    work: FrameWork,
    sampled_at: Duration,
    cursor: Option<CaptureCursor>,
}

enum FrameWork {
    Waiting,
    Readback(CaptureTask),
    Failed(CaptureFailureReason),
}

impl OutputCapture {
    pub(super) fn new(display: &DisplayHandle) -> Self {
        Self {
            sources: OutputCaptureSourceState::new::<ServerState>(display),
            copies: ImageCopyCaptureState::new::<ServerState>(display),
            enabled: false,
            sessions: Vec::new(),
            pending: Vec::new(),
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
        if !self.capture.enabled {
            return None;
        }
        let id = source.user_data().get::<OutputId>()?;
        let output = self.outputs.get(id)?;
        Some(BufferConstraints {
            size: (output.metrics.mode().size.w, output.metrics.mode().size.h).into(),
            shm: vec![wl_shm::Format::Xrgb8888],
            dma: None,
        })
    }

    fn new_session(&mut self, session: Session) {
        if self.capture.sessions.len() < MAX_SESSIONS {
            self.capture.sessions.push(session);
        }
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        let source = session.source();
        let Some(constraints) = self.capture_constraints(&source) else {
            frame.fail(CaptureFailureReason::Stopped);
            return;
        };
        let Some(&output) = source.user_data().get::<OutputId>() else {
            frame.fail(CaptureFailureReason::Stopped);
            return;
        };
        let buffer_matches = with_buffer_contents(&frame.buffer(), |_, _, data| {
            (data.width, data.height) == (constraints.size.w, constraints.size.h)
        })
        .unwrap_or(false);
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
        self.capture.pending.push(PendingFrame {
            output,
            session: session.clone(),
            frame,
            extent: Extent::new(width, height),
            deadline: Instant::now() + FRAME_TIMEOUT,
            work: FrameWork::Waiting,
            sampled_at: Duration::ZERO,
            cursor: None,
        });
    }

    fn frame_aborted(&mut self, frame: FrameRef) {
        self.capture
            .pending
            .retain(|pending| pending.frame != frame);
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.capture
            .pending
            .retain(|pending| pending.session != session);
        self.capture.sessions.retain(|owned| *owned != session);
        self.capture.copies.cleanup();
    }
}

impl ServerState {
    pub(crate) fn enable_output_capture(&mut self) {
        self.capture.enabled = true;
    }

    pub(crate) fn output_capture_requested(&self) -> bool {
        self.capture
            .pending
            .iter()
            .any(|pending| matches!(pending.work, FrameWork::Waiting))
    }

    pub(crate) fn output_capture_timeout(&self, now: Instant) -> Option<Duration> {
        self.capture
            .pending
            .iter()
            .map(|pending| match pending.work {
                FrameWork::Failed(_) => Duration::ZERO,
                _ => pending.deadline.saturating_duration_since(now),
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
        let sampled_at = Clock::<Monotonic>::new().now().into();
        let draw_cursor = self.capture.pending.iter().any(|pending| {
            pending.output == output
                && matches!(pending.work, FrameWork::Waiting)
                && pending.session.draw_cursor()
        });
        let cursor = if draw_cursor {
            self.capture_cursor(output)
        } else {
            Ok(None)
        };
        for pending in &mut self.capture.pending {
            if pending.output != output
                || !matches!(pending.work, FrameWork::Waiting)
                || pending.frame.has_failed()
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

    pub(crate) fn poll_output_captures(&mut self) {
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
            } else {
                self.capture.sessions.retain(|owned| *owned != session);
            }
        }
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
