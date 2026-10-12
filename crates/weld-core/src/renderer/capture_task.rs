//! Bounded screenshot readback and PNG writing outside the compositor host loop.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread,
};

use anyhow::{Context, Result};

use super::{CaptureReadback, read_capture_readback, write_png};
use crate::host::ClientRuntimeNotifier;

pub(crate) struct CaptureTask {
    completion: Receiver<Result<Vec<u8>, String>>,
    cancelled: Arc<AtomicBool>,
}

struct Work {
    device: wgpu::Device,
    capture: CaptureReadback,
    submission: wgpu::SubmissionIndex,
    output: CaptureOutput,
    completion: SyncSender<Result<Vec<u8>, String>>,
    cancelled: Arc<AtomicBool>,
}

enum CaptureOutput {
    Rgba,
    Png,
    File(PathBuf),
}

pub(crate) struct CaptureWorker {
    jobs: SyncSender<Work>,
    wake: ClientRuntimeNotifier,
}

impl CaptureWorker {
    pub(crate) fn new(wake: ClientRuntimeNotifier) -> Result<Self> {
        let (jobs, receiver) = mpsc::sync_channel::<Work>(8);
        let worker_wake = wake.clone();
        thread::Builder::new()
            .name("weld-screenshot".into())
            .spawn(move || {
                for work in receiver {
                    if work.cancelled.load(Ordering::Acquire) {
                        continue;
                    }
                    let result = (|| -> Result<Vec<u8>> {
                        let (width, height) = (work.capture.width, work.capture.height);
                        let pixels =
                            read_capture_readback(&work.device, work.capture, work.submission)?;
                        match work.output {
                            CaptureOutput::File(path) => {
                                if !work.cancelled.load(Ordering::Acquire) {
                                    write_png(&path, width, height, &pixels)?;
                                }
                                Ok(Vec::new())
                            }
                            CaptureOutput::Png => {
                                let mut bytes = Vec::new();
                                super::encode_png(&mut bytes, width, height, &pixels)?;
                                Ok(bytes)
                            }
                            CaptureOutput::Rgba => Ok(pixels),
                        }
                    })()
                    .map_err(|error| format!("{error:#}"));
                    let _ = work.completion.send(result);
                    let _ = worker_wake.notify();
                }
            })
            .context("could not start screenshot readback worker")?;
        Ok(Self { jobs, wake })
    }

    pub(super) fn submit(
        &self,
        device: wgpu::Device,
        capture: CaptureReadback,
        submission: wgpu::SubmissionIndex,
        path: Option<PathBuf>,
    ) -> CaptureTask {
        self.submit_output(
            device,
            capture,
            submission,
            path.map_or(CaptureOutput::Rgba, CaptureOutput::File),
        )
    }

    pub(crate) fn submit_png(
        &self,
        device: wgpu::Device,
        capture: CaptureReadback,
        submission: wgpu::SubmissionIndex,
    ) -> CaptureTask {
        self.submit_output(device, capture, submission, CaptureOutput::Png)
    }

    fn submit_output(
        &self,
        device: wgpu::Device,
        capture: CaptureReadback,
        submission: wgpu::SubmissionIndex,
        output: CaptureOutput,
    ) -> CaptureTask {
        let (sender, completion) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let work = Work {
            device,
            capture,
            submission,
            output,
            completion: sender,
            cancelled: cancelled.clone(),
        };
        if let Err(error) = self.jobs.try_send(work) {
            let (TrySendError::Full(work) | TrySendError::Disconnected(work)) = error;
            let _ = work
                .completion
                .send(Err("screenshot worker unavailable or busy".into()));
            let _ = self.wake.notify();
        }
        CaptureTask {
            completion,
            cancelled,
        }
    }
}

impl CaptureTask {
    pub(crate) fn poll(&self) -> Option<Result<Vec<u8>, String>> {
        match self.completion.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("screenshot worker disconnected".into())),
        }
    }
}

impl Drop for CaptureTask {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::renderer::encode_capture_readback;

    #[test]
    #[ignore = "requires a Vulkan device"]
    fn async_capture_keeps_each_snapshot_when_the_source_texture_is_reused() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter =
            pollster::block_on(instance.request_adapter(&Default::default())).expect("adapter");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&Default::default())).expect("device");
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("capture source reuse regression"),
            size: wgpu::Extent3d {
                width: 9,
                height: 7,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let snapshot = |color: [u8; 4]| {
            queue.write_texture(
                texture.as_image_copy(),
                &color.repeat(9 * 7),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(9 * 4),
                    rows_per_image: Some(7),
                },
                texture.size(),
            );
            let mut encoder = device.create_command_encoder(&Default::default());
            let readback =
                encode_capture_readback(&device, &mut encoder, &texture, 9, 7, texture.format());
            (readback, queue.submit([encoder.finish()]))
        };
        let red = snapshot([255, 0, 0, 255]);
        let blue = snapshot([0, 0, 255, 255]);
        drop(texture);
        let (wake, _source) = crate::host::client_runtime_notifier().expect("completion wake");
        let worker = CaptureWorker::new(wake).expect("worker");
        for ((readback, submission), color) in [(red, [255, 0, 0, 255]), (blue, [0, 0, 255, 255])] {
            let task = worker.submit(device.clone(), readback, submission, None);
            let pixels = task
                .completion
                .recv_timeout(Duration::from_secs(15))
                .expect("completion")
                .expect("pixels");
            assert_eq!(pixels, color.repeat(9 * 7));
        }
    }
}
