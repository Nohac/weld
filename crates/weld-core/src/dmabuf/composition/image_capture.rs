//! Native image-copy destinations over the same window scene renderer as streaming.

use super::{FORMAT, SourceCompositor, sampling_barrier_command};
use crate::{
    dmabuf::sync::record_sampling_barriers,
    host::ClientRuntimeNotifier,
    renderer::{
        capture_gpu::{GpuCapture, GpuCaptureTask},
        capture_task::{CaptureTask, CaptureWorker},
        encode_capture_readback,
    },
};
use anyhow::{Result, ensure};
use smithay::backend::allocator::dmabuf::Dmabuf;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use weld_client::ComposedBuffer;

pub(crate) enum ImageCaptureTask {
    Gpu(GpuCaptureTask),
    Readback(CaptureTask),
}

pub(crate) struct WindowImageCapture {
    compositor: SourceCompositor,
    gpu: GpuCapture,
    // Protocol cancellation cannot release a source still being sampled by the GPU.
    retained: Vec<(Arc<AtomicBool>, ComposedBuffer)>,
}

impl WindowImageCapture {
    pub(crate) fn new(render_node: &Path, wake: ClientRuntimeNotifier) -> Result<Self> {
        let compositor = SourceCompositor::new(render_node)?;
        let gpu = GpuCapture::with_format(&compositor.device, &compositor.queue, wake, FORMAT)?;
        Ok(Self {
            compositor,
            gpu,
            retained: Vec::new(),
        })
    }

    pub(crate) fn prune(&mut self) {
        if !self.retained.is_empty() {
            let _ = self.compositor.device.poll(wgpu::PollType::Poll);
        }
        self.retained
            .retain(|(done, _)| !done.load(Ordering::Acquire));
        self.gpu.prune();
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.retained.is_empty()
    }

    pub(crate) fn submit(
        &mut self,
        frame: &ComposedBuffer,
        buffer: Option<&Dmabuf>,
        worker: &CaptureWorker,
    ) -> Result<ImageCaptureTask> {
        self.prune();
        ensure!(self.retained.len() < 8, "window capture queue is full");
        let compositor = &mut self.compositor;
        let live = compositor.prepare(frame)?;
        let images = compositor.input_images(&live);
        let task = if let Some(buffer) = buffer {
            ImageCaptureTask::Gpu(self.gpu.submit_render(
                buffer,
                |encoder, acquire| {
                    record_sampling_barriers(
                        encoder,
                        &compositor.raw_device,
                        compositor.queue_family,
                        &images,
                        acquire,
                    )
                },
                |_, _, target, encoder| compositor.draw(frame, target, encoder),
            )?)
        } else {
            let acquire = sampling_barrier_command(
                &compositor.device,
                &compositor.raw_device,
                compositor.queue_family,
                &images,
                true,
            )?;
            let release = sampling_barrier_command(
                &compositor.device,
                &compositor.raw_device,
                compositor.queue_family,
                &images,
                false,
            )?;
            let row_bytes = (u64::from(frame.extent.width) * 4).next_multiple_of(256);
            ensure!(
                row_bytes
                    .checked_mul(u64::from(frame.extent.height))
                    .is_some_and(|bytes| bytes <= 64 * 1024 * 1024),
                "window capture exceeds readback budget"
            );
            let texture = compositor.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("window capture readback"),
                size: wgpu::Extent3d {
                    width: frame.extent.width,
                    height: frame.extent.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let mut encoder = compositor
                .device
                .create_command_encoder(&Default::default());
            compositor.draw(
                frame,
                &texture.create_view(&Default::default()),
                &mut encoder,
            )?;
            let readback = encode_capture_readback(
                &compositor.device,
                &mut encoder,
                &texture,
                frame.extent.width,
                frame.extent.height,
                FORMAT,
            );
            let submission = compositor
                .queue
                .submit(acquire.into_iter().chain([encoder.finish()]).chain(release));
            ImageCaptureTask::Readback(worker.submit(
                compositor.device.clone(),
                readback,
                submission,
                None,
            ))
        };
        let done = Arc::new(AtomicBool::new(false));
        let signal = done.clone();
        let wake = worker.notifier();
        compositor.queue.on_submitted_work_done(move || {
            signal.store(true, Ordering::Release);
            let _ = wake.notify();
        });
        self.retained.push((done, frame.clone()));
        Ok(task)
    }
}

impl Drop for WindowImageCapture {
    fn drop(&mut self) {
        // Shutdown joins source sampling before dropping the native-thread leases.
        if !self.retained.is_empty()
            && let Err(error) = self.compositor.device.poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
        {
            tracing::warn!(%error, "retiring window image capture");
        }
    }
}
