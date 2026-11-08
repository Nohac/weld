//! GPU capture destinations and bounded asynchronous completion. Protocol objects
//! stay on the host; submitted imports remain owned until GPU work completes.
use super::CompositionBlitter;
use crate::{
    dmabuf::target::{
        ForeignImageBarrier, ImportedRenderTarget, foreign_image_barrier_command,
        import_capture_target,
    },
    host::ClientRuntimeNotifier,
};
use anyhow::{Context, Result, ensure};
use smithay::backend::allocator::dmabuf::{Dmabuf, WeakDmabuf};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread,
};

const MAX_IN_FLIGHT: usize = 8;
pub(crate) struct GpuCapture {
    device: wgpu::Device,
    queue: wgpu::Queue,
    imports: HashMap<WeakDmabuf, ImportedRenderTarget>,
    blitter: CompositionBlitter,
    cursor_blitter: CompositionBlitter,
    jobs: SyncSender<Completion>,
    active: Arc<AtomicUsize>,
    format: wgpu::TextureFormat,
}
pub(crate) struct GpuCaptureTask(Receiver<Result<(), String>>);
struct Completion {
    device: wgpu::Device,
    submission: wgpu::SubmissionIndex,
    _texture: wgpu::Texture,
    _buffer: Dmabuf,
    done: SyncSender<Result<(), String>>,
    _slot: Slot,
}
struct Slot(Arc<AtomicUsize>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl GpuCapture {
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        wake: ClientRuntimeNotifier,
    ) -> Result<Self> {
        Self::with_format(device, queue, wake, wgpu::TextureFormat::Bgra8UnormSrgb)
    }

    pub(crate) fn with_format(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        wake: ClientRuntimeNotifier,
        format: wgpu::TextureFormat,
    ) -> Result<Self> {
        let (jobs, receiver) = mpsc::sync_channel::<Completion>(MAX_IN_FLIGHT);
        thread::Builder::new()
            .name("weld-capture-gpu".into())
            .spawn(move || {
                for work in receiver {
                    // Even a cancelled request retains its allocation until the write
                    // completes. Cancellation must never release an in-flight import.
                    let result = work
                        .device
                        .poll(wgpu::PollType::Wait {
                            submission_index: Some(work.submission.clone()),
                            timeout: None,
                        })
                        .map(|_| ())
                        .map_err(|error| error.to_string());
                    let _ = work.done.send(result);
                    drop(work);
                    let _ = wake.notify();
                }
            })
            .context("starting GPU capture completion worker")?;
        Ok(Self {
            format,
            device: device.clone(),
            queue: queue.clone(),
            imports: HashMap::new(),
            blitter: CompositionBlitter::new(device, format),
            cursor_blitter: CompositionBlitter::straight_alpha(device, format),
            jobs,
            active: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub(crate) fn prune(&mut self) {
        self.imports.retain(|buffer, _| !buffer.is_gone());
    }

    pub(crate) fn submit(
        &mut self,
        buffer: &Dmabuf,
        source: &wgpu::TextureView,
        overlay: impl FnOnce(
            &CompositionBlitter,
            &wgpu::TextureView,
            &mut wgpu::CommandEncoder,
        ) -> Result<()>,
    ) -> Result<GpuCaptureTask> {
        let device = self.device.clone();
        self.submit_render(buffer, None, None, |blitter, cursor, target, encoder| {
            let binding = blitter.create_bind_group(&device, "capture composition", source);
            blitter.encode(encoder, "output capture copy", target, &binding);
            overlay(cursor, target, encoder)
        })
    }

    /// Sources and destination acquire/release on the same queue submission.
    pub(crate) fn submit_render(
        &mut self,
        buffer: &Dmabuf,
        before: Option<wgpu::CommandBuffer>,
        after: Option<wgpu::CommandBuffer>,
        render: impl FnOnce(
            &CompositionBlitter,
            &CompositionBlitter,
            &wgpu::TextureView,
            &mut wgpu::CommandEncoder,
        ) -> Result<()>,
    ) -> Result<GpuCaptureTask> {
        let reserved = self
            .active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_IN_FLIGHT).then_some(count + 1)
            });
        ensure!(reserved.is_ok(), "GPU capture queue is full");
        let slot = Slot(self.active.clone());
        self.prune();
        let key = buffer.weak();
        if !self.imports.contains_key(&key) {
            ensure!(self.imports.len() < 64, "capture import cache is full");
            self.imports.insert(
                key.clone(),
                import_capture_target(&self.device, buffer, self.format)?,
            );
        }
        let target = self
            .imports
            .get(&key)
            .context("capture import disappeared")?;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("DMA-BUF image capture"),
            });
        render(
            &self.blitter,
            &self.cursor_blitter,
            &target.view,
            &mut encoder,
        )?;
        // SAFETY: the imported image and raw device belong to this wgpu device.
        // The completion job retains the allocation through acquire/draw/release.
        // The host has waited for foreign readers before calling submit.
        let (acquire, release) = unsafe {
            let raw = self
                .device
                .as_hal::<wgpu::hal::api::Vulkan>()
                .context("capture device is not Vulkan")?;
            (
                foreign_image_barrier_command(
                    &self.device,
                    raw.raw_device(),
                    raw.queue_family_index(),
                    target.image,
                    true,
                    ForeignImageBarrier::Acquire,
                )?,
                foreign_image_barrier_command(
                    &self.device,
                    raw.raw_device(),
                    raw.queue_family_index(),
                    target.image,
                    true,
                    ForeignImageBarrier::Release,
                )?,
            )
        };
        let submission = self.queue.submit(
            before
                .into_iter()
                .chain([acquire, encoder.finish(), release])
                .chain(after),
        );
        let (done, result) = mpsc::sync_channel(1);
        let work = Completion {
            device: self.device.clone(),
            submission,
            _texture: target.texture.clone(),
            _buffer: buffer.clone(),
            done,
            _slot: slot,
        };
        // Every queued job owns a reserved slot; capacity cannot be exhausted.
        // If the worker terminated, retain the job until wgpu reports completion.
        if let Err(error) = self.jobs.try_send(work) {
            let (mpsc::TrySendError::Full(work) | mpsc::TrySendError::Disconnected(work)) = error;
            self.queue.on_submitted_work_done(move || {
                let _ = work.done.send(Ok(()));
                drop(work);
            });
        }
        Ok(GpuCaptureTask(result))
    }
}
impl GpuCaptureTask {
    pub(crate) fn poll(&self) -> Option<Result<(), String>> {
        match self.0.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err("capture completion worker disconnected".into()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        dmabuf::{request_weld_device, target::import_render_target},
        host::{CompositionFrame, CompositionTargetView, client_runtime_notifier},
        renderer::{capture_snapshot::snapshot_composition, read_owned_frame_rgba},
        surface::Extent,
    };
    use smithay::backend::allocator::{
        Allocator, Fourcc, Modifier,
        dmabuf::{AsDmabuf, DmabufMappingMode, DmabufSyncFlags},
        gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
    };
    use std::{fs::File, time::Duration};

    #[test]
    #[ignore = "requires a Vulkan GPU and accessible DRM render node"]
    fn dmabuf_capture_preserves_pixels_across_foreign_target_reuse() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle().with_env()
        });
        let adapter =
            pollster::block_on(instance.request_adapter(&Default::default())).expect("adapter");
        let (device, queue, capabilities) =
            request_weld_device(&adapter, "capture regression").expect("device");
        let capabilities = capabilities.expect("DMA-BUF support");
        let node = capabilities
            .external_imports()
            .expect("render node")
            .render_node;
        let file = File::options()
            .read(true)
            .write(true)
            .open(node)
            .expect("render node");
        let mut allocator = GbmAllocator::new(
            GbmDevice::new(file).expect("GBM"),
            GbmBufferFlags::RENDERING,
        );
        let allocation = allocator
            .create_buffer(32, 24, Fourcc::Argb8888, &[Modifier::Linear])
            .expect("source allocation");
        let source_buffer = allocation.export().expect("export source");
        let source =
            import_render_target(&device, &source_buffer, wgpu::TextureFormat::Bgra8UnormSrgb)
                .expect("source import");
        let destination_allocation = allocator
            .create_buffer(32, 24, Fourcc::Xrgb8888, &[Modifier::Linear])
            .expect("destination allocation");
        let destination = destination_allocation.export().expect("export destination");
        let (wake, _events) = client_runtime_notifier().expect("wake");
        let mut capture = GpuCapture::new(&device, &queue, wake).expect("capture");
        let mut snapshot = None;
        for (color, expected_bgra) in [
            (wgpu::Color::RED, [0, 0, 255, 255]),
            (wgpu::Color::BLUE, [255, 0, 0, 255]),
            (wgpu::Color::GREEN, [0, 255, 0, 255]),
        ] {
            // SAFETY: the private GBM image is retained here and only this test
            // acquires/renders/releases it on the owning queue, like DRM output.
            let (acquire, release) = unsafe {
                let raw = device.as_hal::<wgpu::hal::api::Vulkan>().expect("Vulkan");
                (
                    foreign_image_barrier_command(
                        &device,
                        raw.raw_device(),
                        raw.queue_family_index(),
                        source.image,
                        true,
                        ForeignImageBarrier::Acquire,
                    )
                    .expect("acquire"),
                    foreign_image_barrier_command(
                        &device,
                        raw.raw_device(),
                        raw.queue_family_index(),
                        source.image,
                        true,
                        ForeignImageBarrier::Release,
                    )
                    .expect("release"),
                )
            };
            let mut encoder = device.create_command_encoder(&Default::default());
            {
                let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &source.view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(color),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
            }
            queue.submit([acquire, encoder.finish()]);
            let frame = CompositionFrame::external(CompositionTargetView::new(
                source.view.clone(),
                Extent::new(32, 24),
                source.texture.format(),
            ));
            let retained =
                snapshot_composition(&device, &queue, &frame, snapshot.take()).expect("snapshot");
            queue.submit([release]);
            let task = capture
                .submit(&destination, retained.target().view(), |_, _, _| Ok(()))
                .expect("capture submission");
            task.0
                .recv_timeout(Duration::from_secs(5))
                .expect("completion timeout")
                .expect("GPU completion");
            destination
                .sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::READ)
                .expect("map sync");
            let mapping = destination
                .map_plane(0, DmabufMappingMode::READ)
                .expect("map");
            let stride = destination.strides().next().expect("stride") as usize;
            assert!(mapping.length() >= stride * 23 + 128);
            for row in 0..24 {
                // SAFETY: our exclusive allocation is GPU-complete and the row
                // bounds were checked against the live CPU mapping above.
                let pixels = unsafe {
                    std::slice::from_raw_parts(mapping.ptr().cast::<u8>().add(row * stride), 128)
                };
                for pixel in pixels.as_chunks::<4>().0 {
                    assert_eq!(&pixel[..3], &expected_bgra[..3]);
                }
            }
            destination
                .sync_plane(0, DmabufSyncFlags::END | DmabufSyncFlags::READ)
                .expect("unmap sync");
            let mut expected_rgba = expected_bgra;
            expected_rgba.swap(0, 2);
            assert_eq!(
                read_owned_frame_rgba(&device, &queue, &retained).expect("snapshot pixels"),
                expected_rgba.repeat(32 * 24)
            );
            snapshot = Some(retained);
        }
        assert_eq!(
            capture.imports.len(),
            1,
            "repeated captures reuse the import"
        );
    }
}
