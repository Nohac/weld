//! On-demand snapshots use the source compositor's crop, alpha and popup geometry.

use super::{FORMAT, SourceCompositor, sampling_barrier_command};
use crate::{
    host::ClientRuntimeNotifier,
    renderer::{
        capture_task::{CaptureTask, CaptureWorker},
        encode_capture_readback,
    },
};
use anyhow::{Context, Result, ensure};
use std::path::Path;
use weld_client::ComposedBuffer;

pub struct SourceCapture {
    compositor: SourceCompositor,
    worker: CaptureWorker,
}

/// Retains source leases on the native thread through GPU sampling completion.
pub struct SourceCaptureTask {
    task: CaptureTask,
    device: wgpu::Device,
    submission: Option<wgpu::SubmissionIndex>,
    frame: Option<ComposedBuffer>,
}

impl SourceCapture {
    pub fn new(render_node: &Path, wake: ClientRuntimeNotifier) -> Result<Self> {
        Ok(Self {
            compositor: SourceCompositor::new(render_node)?,
            worker: CaptureWorker::new(wake)?,
        })
    }

    pub fn capture(&mut self, frame: &ComposedBuffer) -> Result<SourceCaptureTask> {
        let row_bytes = (u64::from(frame.extent.width) * 4).next_multiple_of(256);
        ensure!(
            row_bytes
                .checked_mul(u64::from(frame.extent.height))
                .is_some_and(|bytes| bytes <= 64 * 1024 * 1024),
            "window snapshot exceeds the 64 MiB readback budget; reduce the application viewport"
        );
        let compositor = &mut self.compositor;
        let live = compositor.prepare(frame)?;
        let images = compositor.input_images(&live);
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
        let texture = compositor.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("window snapshot"),
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
        let task = self
            .worker
            .submit_png(compositor.device.clone(), readback, submission.clone());
        Ok(SourceCaptureTask {
            task,
            device: compositor.device.clone(),
            submission: Some(submission),
            frame: Some(frame.clone()),
        })
    }
}

impl SourceCaptureTask {
    pub fn poll(&mut self) -> Option<Result<Vec<u8>>> {
        let result = self.task.poll()?;
        // A failed readback may still have submitted sampling work. Keep the leases
        // until Drop joins that submission; successful readback proves completion.
        if result.is_ok() {
            self.submission = None;
            self.frame = None;
        }
        Some(result.map_err(anyhow::Error::msg))
    }
}

impl Drop for SourceCaptureTask {
    fn drop(&mut self) {
        if let Some(submission) = self.submission.take() {
            // Cancellation and shutdown join sampling before releasing client storage.
            if let Err(error) = self
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(submission),
                    timeout: None,
                })
                .context("retiring window capture")
            {
                tracing::warn!(%error, "window capture device failed");
            }
        }
    }
}
