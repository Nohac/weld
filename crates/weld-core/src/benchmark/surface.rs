//! Fixed-row presenter for benchmark toplevels using the production DMA-BUF manager.

use crate::{
    ApplicationHost, CompositionDemand, CompositionHost, HostPolicy, OutputConfiguration, OutputId,
    RenderContext,
    cursor::CursorHostUpdate,
    dmabuf::{DmabufManager, ImportId, ImportedImageRegistry, PromotionImage},
    host::{
        CaptureRequest, CompositionFrame, CompositionOutputFrame, CompositionOutputRequest,
        CompositionTargetView,
    },
    input::{InputPosition, RawSeatEvent, RawSeatEventKind},
    renderer::CompositionBlitter,
    runtime::HostCommand,
    surface::{SurfaceId, SurfaceLayerId},
};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, HashMap, HashSet};
use weld_client::{
    ClientAdapterCommandEnvelope, ClientBufferUseId, ClientEventQueue, ClientFocusRequest,
    ClientPointerRoute, ClientPointerRouteUpdate, ClientRequest, ClientSurfaceEvent,
    ClientSurfaceEventKind, InputTransform, SurfaceBufferChange,
};

struct Images {
    device: wgpu::Device,
    blitter: CompositionBlitter,
    bindings: HashMap<ImportId, wgpu::BindGroup>,
}
impl ImportedImageRegistry for Images {
    fn install(&mut self, images: &[PromotionImage]) -> Result<()> {
        for image in images {
            self.bindings.entry(image.id).or_insert_with(|| {
                self.blitter
                    .create_bind_group(&self.device, "benchmark client", &image.view)
            });
        }
        Ok(())
    }
    fn prune(&mut self, images: &[ImportId]) {
        for image in images {
            self.bindings.remove(image);
        }
    }
}

struct Window {
    image: ImportId,
    layer: SurfaceLayerId,
    width: u32,
    height: u32,
}

pub struct SurfaceOnly {
    context: RenderContext,
    manager: DmabufManager,
    images: Images,
    windows: BTreeMap<SurfaceId, Window>,
    pending: ClientEventQueue,
    target: wgpu::Texture,
    view: CompositionTargetView,
    position: InputPosition,
    time: u32,
    routes: Vec<ClientPointerRouteUpdate>,
    last_route: Option<ClientPointerRoute>,
    requests: Vec<ClientRequest>,
    error: Option<String>,
    frame_work: Option<Box<dyn FnMut()>>,
}

impl SurfaceOnly {
    /// Add independently owned policy work while retaining the direct renderer.
    pub fn with_frame_work(mut self, work: impl FnMut() + 'static) -> Self {
        self.frame_work = Some(Box::new(work));
        self
    }

    pub fn new(context: RenderContext) -> Result<Self> {
        let extent = context.outputs.first().context("missing output")?.extent();
        let target = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("surface-only output"),
            size: wgpu::Extent3d {
                width: extent.width,
                height: extent.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: context.composition_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = CompositionTargetView::new(
            target.create_view(&Default::default()),
            extent,
            context.composition_format,
        );
        let manager = context
            .dmabuf
            .create_manager(&context.device, &context.queue)?
            .context("missing DMA-BUF manager")?;
        let images = Images {
            device: context.device.clone(),
            blitter: CompositionBlitter::for_benchmark_client(
                &context.device,
                context.composition_format,
            ),
            bindings: HashMap::new(),
        };
        Ok(Self {
            context,
            manager,
            images,
            windows: BTreeMap::new(),
            pending: ClientEventQueue::default(),
            target,
            view,
            position: InputPosition::default(),
            time: 0,
            routes: Vec::new(),
            last_route: None,
            requests: Vec::new(),
            error: None,
            frame_work: None,
        })
    }
    fn consume(&mut self, event: ClientSurfaceEvent) -> Result<()> {
        match event.kind {
            ClientSurfaceEventKind::Commit(commit) if commit.mapped => {
                let commit = commit.into_state();
                ensure!(
                    commit.overlays.is_empty(),
                    "surface-only workload must have one root layer per window"
                );
                for buffer in commit.buffers {
                    if let SurfaceBufferChange::Replaced { buffer: lease, .. } = buffer.change {
                        let imported = self.manager.stage(event.surface, buffer.layer, lease)?;
                        ensure!(
                            !imported.y_inverted,
                            "surface-only benchmark does not support inverted buffers"
                        );
                        let first = self.windows.is_empty();
                        self.windows.insert(
                            event.surface,
                            Window {
                                image: imported.id,
                                layer: buffer.layer,
                                width: imported.extent.width,
                                height: imported.extent.height,
                            },
                        );
                        if first {
                            self.requests.push(ClientRequest::Focus(ClientFocusRequest {
                                source: event.surface.source(),
                                surface: Some(event.surface),
                            }));
                        }
                    }
                }
            }
            ClientSurfaceEventKind::Commit(_) | ClientSurfaceEventKind::Destroyed => {
                self.windows.remove(&event.surface);
                self.manager.remove_surface(event.surface);
            }
            _ => {}
        }
        Ok(())
    }
}
impl ApplicationHost for SurfaceOnly {
    fn composition(&mut self) -> Option<&mut dyn CompositionHost> {
        Some(self)
    }
}
impl HostPolicy for SurfaceOnly {
    fn enqueue_client_event(&mut self, event: ClientSurfaceEvent) -> CompositionDemand {
        self.pending.push(event);
        CompositionDemand::Ordinary
    }
    fn enqueue_input_event(&mut self, event: RawSeatEvent) -> bool {
        if let RawSeatEventKind::PointerMotion { position } = event.event {
            self.position = position;
        }
        self.time = event.time;
        true
    }
    fn advance_main(&mut self, _: u32) -> bool {
        if let Some(work) = &mut self.frame_work {
            work();
        }
        while let Some(event) = self.pending.pop_front() {
            if let Err(error) = self.consume(event) {
                self.error = Some(error.to_string());
            }
        }
        let width = f64::from(self.view.extent().width) / self.windows.len().max(1) as f64;
        let route = self
            .windows
            .iter()
            .enumerate()
            .find(|(index, _)| {
                self.position.x >= *index as f64 * width
                    && self.position.x < (*index + 1) as f64 * width
            })
            .map(|(index, (surface, window))| {
                let scale = f64::from(window.width) / width;
                ClientPointerRoute {
                    surface: *surface,
                    layer: window.layer,
                    transform: InputTransform {
                        xx: scale,
                        yy: f64::from(window.height) / f64::from(self.view.extent().height),
                        x: -(index as f64 * width) * scale,
                        ..InputTransform::IDENTITY
                    },
                }
            });
        if self.last_route != route {
            self.last_route = route;
            self.routes.push(ClientPointerRouteUpdate {
                route,
                position: self.position,
                time: self.time,
            });
        }
        false
    }
    fn service_remote_debug(&mut self) {}
    fn update_output_topology(&mut self, _: &[OutputConfiguration]) {}
    fn should_exit(&self) -> bool {
        false
    }
    fn take_pointer_route_updates(&mut self) -> Vec<ClientPointerRouteUpdate> {
        std::mem::take(&mut self.routes)
    }
    fn take_cursor_update(&mut self) -> CursorHostUpdate {
        CursorHostUpdate::default()
    }
    fn take_host_commands(&mut self) -> Vec<HostCommand> {
        Vec::new()
    }
    fn take_virtual_terminal_switch_request(&mut self) -> Option<i32> {
        None
    }
    fn take_client_requests(&mut self) -> Vec<ClientRequest> {
        std::mem::take(&mut self.requests)
    }
    fn take_adapter_commands(&mut self) -> Vec<ClientAdapterCommandEnvelope> {
        Vec::new()
    }
}
impl CompositionHost for SurfaceOnly {
    fn render_outputs(
        &mut self,
        _: &[CompositionOutputRequest],
        frames: &mut Vec<CompositionOutputFrame>,
    ) -> Result<()> {
        if let Some(error) = self.error.take() {
            anyhow::bail!(error);
        }
        let referenced = self
            .windows
            .values()
            .map(|window| window.image)
            .collect::<HashSet<_>>();
        self.manager.prepare_render(&referenced, &mut self.images)?;
        let mut encoder = self
            .context
            .device
            .create_command_encoder(&Default::default());
        {
            let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("surface-only clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: self.view.view(),
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        let extent = self.view.extent();
        let width = extent.width as f32 / self.windows.len().max(1) as f32;
        for (index, window) in self.windows.values().enumerate() {
            let binding = self
                .images
                .bindings
                .get(&window.image)
                .context("missing promoted image")?;
            self.images.blitter.encode_overlay(
                &mut encoder,
                "surface-only client",
                self.view.view(),
                binding,
                (index as f32 * width, 0.0, width, extent.height as f32),
                [(0, 0, extent.width, extent.height)],
            );
        }
        self.context.queue.submit([encoder.finish()]);
        self.manager.finish_render(&mut self.images)?;
        frames.clear();
        frames.push(CompositionOutputFrame {
            output: OutputId::new(1),
            frame: CompositionFrame::owned(self.view.clone(), self.target.clone()),
        });
        Ok(())
    }
    fn complete_dmabuf_uses(&mut self, uses: &[ClientBufferUseId]) {
        self.manager.complete_gpu_uses(uses);
    }
    fn has_surface_frame(&self) -> bool {
        !self.windows.is_empty()
    }
    fn take_capture_request(&mut self) -> Option<CaptureRequest> {
        None
    }
    fn complete_capture(&mut self, _: u64, _: Result<(), String>) {}
}
