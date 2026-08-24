//! Bevy-owned compositor scene rendered into a Weld-owned wgpu texture.

use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use bevy::{
    app::{App, Plugin, PluginGroup, PostUpdate, TerminalCtrlCHandlerPlugin},
    camera::{
        Camera, Camera2d, ClearColorConfig, CompositingSpace, ManualTextureViewHandle,
        NormalizedRenderTarget, RenderTarget,
    },
    ecs::{
        change_detection::DetectChangesMut,
        message::{MessageCursor, Messages},
        resource::Resource,
        schedule::IntoScheduleConfigs,
        world::World,
    },
    log::LogPlugin,
    math::{UVec2, Vec2},
    prelude::{ChildOf, Color, DefaultPlugins, Entity, LayoutConfig, Node, With, Without},
    remote::RemoteLast,
    render::{
        RenderApp, RenderPlugin,
        renderer::{
            RenderAdapter, RenderAdapterInfo, RenderDevice, RenderInstance, RenderQueue,
            WgpuWrapper,
        },
        settings::RenderCreation,
        texture::{ManualTextureView, ManualTextureViews},
    },
    time::TimeReceiver,
    ui::{IsDefaultUiCamera, UiScale, UiSystems},
    window::{ExitCondition, RequestRedraw, WindowPlugin},
};

use crate::client::ClientAdapterCommandQueue;
use crate::cursor::{CursorHostTracker, CursorPlugin, take_cursor_update};
use crate::debug::{complete_capture, take_capture_request};
use crate::dmabuf::DmabufImporter;
use crate::input::{
    ApplicationInputBuffer, InputBridgePlugin, InputOutputTarget, enqueue_application_input_batch,
    set_input_update_time, take_host_commands, take_input_effects,
    take_virtual_terminal_switch_request,
};
use crate::output::{
    OutputGeometry, OutputId, OutputInfo, OutputPlacement, OutputPosition, PrimaryOutput,
    RendersOutput, WeldOutput,
};
use crate::surface::{
    HostSurfaceEvent, HostSurfaceEventKind, SurfaceAction, SurfaceBufferContent,
    SurfaceBufferUpdate, SurfaceContentView, SurfaceInputPlacement, SurfaceInputRect,
    SurfaceLayerPlacement, SurfaceTreeSnapshot, SurfaceWindowGeometry, enqueue_surface_event,
    has_surface_frame, publish_surface_bindings, take_surface_actions,
};
use weld_client::{
    ClientFocusRequest, ClientImporterRegistration, ClientOutputId, ClientRequest, ClientSourceId,
    ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceRequest, ClientSurfaceRequestKind,
    SurfaceBufferChange,
};
use weld_core::host::{
    CaptureRequest, CompositionDestination, CompositionFrame, CompositionOutputFrame,
    CompositionOutputRequest, CompositionTargetView, RenderContext,
};
use weld_core::input::RawSeatEvent;
use weld_core::runtime::HostCommand;
use weld_core::surface::Extent;
use weld_core::{
    CompositionDemand, CompositionHost, OutputConfiguration, OutputHead,
    dmabuf::DirectClientBufferAccess,
};

#[cfg(test)]
const PRIMARY_OUTPUT_ID: OutputId = OutputId::new(1);
pub struct AppShell {
    app: App,
    device: wgpu::Device,
    outputs: HashMap<OutputId, AppOutput>,
    redraw_requests: RedrawRequests,
    dmabuf_importer: Option<DmabufImporter>,
    surface_demand: SurfaceCompositionDemand,
    cursor: CursorHostTracker,
    pending_input: ApplicationInputBuffer,
    client_importers: HashSet<ClientSourceId>,
}

struct AppOutput {
    configuration: OutputConfiguration,
    entity: Entity,
    camera: Entity,
    view: ManualTextureViewHandle,
    owned_target: OwnedCompositionTarget,
}

#[derive(Clone, Resource)]
struct CompositionViews(HashMap<OutputId, ManualTextureViewHandle>);

struct OwnedCompositionTarget {
    texture: wgpu::Texture,
    target: CompositionTargetView,
}

#[derive(Clone, Copy)]
struct CompositionTargetContract {
    extent: Extent,
    format: wgpu::TextureFormat,
}

enum ClientBufferAccessResolution {
    Direct(Rc<DirectClientBufferAccess>),
    Unregistered,
    SourceMismatch,
    Unsupported,
}

fn resolve_client_buffer_access(
    importers: &HashSet<ClientSourceId>,
    surface: crate::surface::SurfaceId,
    lease: &weld_client::ClientBufferLease,
) -> ClientBufferAccessResolution {
    if !importers.contains(&surface.source()) {
        return ClientBufferAccessResolution::Unregistered;
    }
    if lease.buffer().source() != surface.source() {
        return ClientBufferAccessResolution::SourceMismatch;
    }
    lease
        .access_rc::<DirectClientBufferAccess>()
        .map(ClientBufferAccessResolution::Direct)
        .unwrap_or(ClientBufferAccessResolution::Unsupported)
}

fn validate_external_target(
    output: OutputId,
    expected: CompositionTargetContract,
    actual: CompositionTargetContract,
) -> Result<()> {
    if actual.extent != expected.extent {
        bail!(
            "external composition target for {output:?} has extent {:?}, expected {:?}",
            actual.extent,
            expected.extent
        );
    }
    if actual.format != expected.format {
        bail!(
            "external composition target for {output:?} has format {:?}, expected {:?}",
            actual.format,
            expected.format
        );
    }
    Ok(())
}

impl OwnedCompositionTarget {
    fn new(device: &wgpu::Device, extent: Extent, format: wgpu::TextureFormat) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("weld Bevy composition target"),
            size: wgpu::Extent3d {
                width: extent.width,
                height: extent.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            target: CompositionTargetView::new(view, extent, format),
        }
    }

    fn frame(&self) -> CompositionFrame {
        CompositionFrame::owned(self.target.clone(), self.texture.clone())
    }
}

#[derive(Default)]
struct SurfaceCompositionDemand {
    mapped_surfaces: HashSet<crate::surface::SurfaceId>,
}

impl SurfaceCompositionDemand {
    fn classify(&mut self, event: &ClientSurfaceEvent) -> CompositionDemand {
        let surface = event.surface;
        match &event.kind {
            ClientSurfaceEventKind::Commit(snapshot) if snapshot.mapped => {
                if self.mapped_surfaces.insert(surface) {
                    CompositionDemand::Settle
                } else {
                    CompositionDemand::Ordinary
                }
            }
            ClientSurfaceEventKind::Commit(_) => {
                if self.mapped_surfaces.remove(&surface) {
                    CompositionDemand::Settle
                } else {
                    CompositionDemand::Ordinary
                }
            }
            ClientSurfaceEventKind::Destroyed => {
                self.mapped_surfaces.remove(&surface);
                CompositionDemand::Settle
            }
            ClientSurfaceEventKind::Interaction(_) => CompositionDemand::Ordinary,
            ClientSurfaceEventKind::Role(_) => CompositionDemand::Settle,
        }
    }
}

/// Installs Weld's application model without selecting a window policy.
pub(crate) struct WeldAppPlugin {
    outputs: Vec<OutputConfiguration>,
    output_info: HashMap<OutputId, OutputInfo>,
    views: CompositionViews,
}

impl WeldAppPlugin {
    pub(crate) fn new(outputs: Vec<OutputConfiguration>, heads: Vec<OutputHead>) -> Result<Self> {
        let primary = outputs
            .iter()
            .copied()
            .find(|output| output.is_primary())
            .map(OutputConfiguration::id)
            .context("render context contains no primary output")?;
        let views = outputs
            .iter()
            .enumerate()
            .map(|(index, output)| {
                let handle = u32::try_from(index + 1)
                    .context("too many outputs for Bevy manual texture-view handles")?;
                Ok((output.id(), ManualTextureViewHandle(handle)))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        views
            .get(&primary)
            .context("primary output has no composition view")?;
        let mut output_info = HashMap::with_capacity(heads.len());
        for head in heads {
            let id = head.id();
            if output_info
                .insert(id, OutputInfo::from_head(&head))
                .is_some()
            {
                bail!("render context contains duplicate output head {id:?}");
            }
        }
        if output_info.len() != outputs.len()
            || outputs
                .iter()
                .any(|output| !output_info.contains_key(&output.id()))
        {
            bail!("render context output heads do not match enabled outputs");
        }
        Ok(Self {
            outputs,
            output_info,
            views: CompositionViews(views),
        })
    }
}

impl Plugin for WeldAppPlugin {
    fn build(&self, app: &mut App) {
        for output in &self.outputs {
            let mut entity = app.world_mut().spawn((
                WeldOutput { id: output.id() },
                self.output_info[&output.id()].clone(),
                OutputGeometry::new(output.extent(), output.scale().value()),
                OutputPlacement::from_configuration(*output),
                OutputPosition(Vec2::new(output.position().x, output.position().y)),
            ));
            if output.is_primary() {
                entity.insert(PrimaryOutput);
            }
        }
        let input_targets = self
            .outputs
            .iter()
            .map(|configuration| InputOutputTarget {
                configuration: *configuration,
                target: NormalizedRenderTarget::TextureView(self.views.0[&configuration.id()]),
            })
            .collect();
        app.insert_resource(self.views.clone());
        app.init_resource::<ClientAdapterCommandQueue>();
        app.add_plugins((
            CursorPlugin,
            crate::surface::SurfacePlugin,
            InputBridgePlugin::new(input_targets),
        ))
        .add_systems(
            PostUpdate,
            disable_ui_rounding_on_roots.before(UiSystems::Layout),
        )
        .insert_resource(UiScale(1.0));
    }
}

/// Install Bevy's renderer against the device opened by the native backend.
pub fn configure_rendering(app: &mut App, context: &RenderContext) {
    let render_creation = RenderCreation::manual(
        RenderDevice::from(context.device.clone()),
        RenderQueue(Arc::new(WgpuWrapper::new(context.queue.clone()))),
        RenderAdapterInfo(WgpuWrapper::new(context.adapter.get_info())),
        RenderAdapter(Arc::new(WgpuWrapper::new(context.adapter.clone()))),
        RenderInstance(Arc::new(WgpuWrapper::new(context.instance.clone()))),
    );
    let render_plugin = RenderPlugin {
        render_creation,
        synchronous_pipeline_compilation: true,
        ..Default::default()
    };
    let window_plugin = WindowPlugin {
        primary_window: None,
        exit_condition: ExitCondition::DontExit,
        ..Default::default()
    };

    app.add_plugins(
        DefaultPlugins
            .set(window_plugin)
            .set(render_plugin)
            .disable::<LogPlugin>()
            .disable::<TerminalCtrlCHandlerPlugin>(),
    );
}

impl AppShell {
    pub fn new(
        mut app: App,
        context: RenderContext,
        importers: Vec<ClientImporterRegistration>,
    ) -> Result<Self> {
        let _startup_span =
            tracing::trace_span!(target: crate::PROFILE_TARGET, "weld_app_shell_startup").entered();

        let mut client_importers = HashSet::new();
        for importer in importers {
            let source = importer.descriptor.id;
            if !crate::surface::register_client_source(app.world_mut(), importer.descriptor) {
                bail!(
                    "client source {} is registered more than once",
                    source.raw()
                );
            }
            if importer
                .importer
                .is::<weld_core::server::WaylandClientImporter>()
                || importer
                    .importer
                    .is::<weld_client::PassthroughClientImporter>()
            {
                client_importers.insert(source);
            } else {
                tracing::warn!(
                    source = source.raw(),
                    "registered client source has no supported buffer importer"
                );
            }
        }

        app.finish();
        app.cleanup();
        app.get_sub_app(RenderApp).context(
            "Bevy RenderPlugin did not create the non-pipelined RenderApp required by Weld",
        )?;
        disconnect_render_time(&mut app)?;

        let output_entities = output_entities(app.world_mut())?;
        let views = app
            .world()
            .get_resource::<CompositionViews>()
            .cloned()
            .context("WeldAppPlugin did not register composition views")?;
        let mut outputs = HashMap::with_capacity(context.outputs.len());
        for configuration in &context.outputs {
            let output = configuration.id();
            let entity = output_entities
                .get(&output)
                .copied()
                .with_context(|| format!("WeldAppPlugin did not create output {output:?}"))?;
            let view = views.0.get(&output).copied().with_context(|| {
                format!("WeldAppPlugin did not assign output {output:?} a view")
            })?;
            let owned_target = OwnedCompositionTarget::new(
                &context.device,
                configuration.extent(),
                context.composition_format,
            );
            insert_manual_view(
                &mut app,
                view,
                &owned_target.target,
                configuration.scale().value(),
            );
            let camera =
                spawn_compositor_camera(app.world_mut(), entity, view, configuration.is_primary());
            outputs.insert(
                output,
                AppOutput {
                    configuration: *configuration,
                    entity,
                    camera,
                    view,
                    owned_target,
                },
            );
        }

        let redraw_requests = app
            .world()
            .get_resource::<Messages<RequestRedraw>>()
            .map(RedrawRequests::new)
            .context("Bevy WindowPlugin did not register redraw messages")?;
        let dmabuf_importer =
            DmabufImporter::new(&context.device, &context.queue, &context.dmabuf)?;

        Ok(Self {
            app,
            device: context.device,
            outputs,
            redraw_requests,
            dmabuf_importer,
            surface_demand: SurfaceCompositionDemand::default(),
            cursor: CursorHostTracker::default(),
            pending_input: ApplicationInputBuffer::default(),
            client_importers,
        })
    }

    /// Advance Bevy policy once for the composition frame being prepared.
    pub fn advance_main(&mut self, input_time: u32) -> bool {
        let _advance_span =
            tracing::trace_span!(target: crate::PROFILE_TARGET, "weld_app_advance_composition")
                .entered();
        enqueue_application_input_batch(self.app.world_mut(), &mut self.pending_input);
        set_input_update_time(self.app.world_mut(), input_time);
        advance_main_app(&mut self.app, &mut self.redraw_requests)
    }

    pub fn service_remote_debug(&mut self) {
        let _ = self.app.world_mut().try_run_schedule(RemoteLast);
    }

    /// Extract the preceding main-world advance and render Weld's composition.
    ///
    /// Construction pins Weld to Bevy's current non-pipelined [`RenderApp`].
    /// Main-world trackers are cleared only after extraction has observed the
    /// refresh-paced application frame. `frames` retains its allocation across
    /// calls and contains a complete composition only when this returns `Ok`.
    pub fn render_outputs(
        &mut self,
        requests: &[CompositionOutputRequest],
        frames: &mut Vec<CompositionOutputFrame>,
    ) -> Result<()> {
        let _composition_span =
            tracing::trace_span!(target: crate::PROFILE_TARGET, "weld_render_composition")
                .entered();

        frames.clear();
        if requests.is_empty() {
            bail!("composition contains no output requests");
        }
        for output in self.outputs.values() {
            let Some(mut camera) = self.app.world_mut().get_mut::<Camera>(output.camera) else {
                bail!(
                    "composition camera for output {:?} disappeared",
                    output.configuration.id()
                );
            };
            camera.is_active = false;
        }
        for (index, request) in requests.iter().enumerate() {
            if requests[..index]
                .iter()
                .any(|previous| previous.output == request.output)
            {
                bail!(
                    "composition requested output {:?} more than once",
                    request.output
                );
            }
            let output = self.outputs.get(&request.output).with_context(|| {
                format!("composition requested unknown output {:?}", request.output)
            })?;
            let frame = match &request.destination {
                CompositionDestination::Owned => output.owned_target.frame(),
                CompositionDestination::External(target) => {
                    validate_external_target(
                        request.output,
                        CompositionTargetContract {
                            extent: output.configuration.extent(),
                            format: output.owned_target.target.format(),
                        },
                        CompositionTargetContract {
                            extent: target.extent(),
                            format: target.format(),
                        },
                    )?;
                    CompositionFrame::external(target.clone())
                }
            };
            insert_manual_view(
                &mut self.app,
                output.view,
                frame.target(),
                output.configuration.scale().value(),
            );
            let Some(mut camera) = self.app.world_mut().get_mut::<Camera>(output.camera) else {
                bail!(
                    "composition camera for output {:?} disappeared",
                    request.output
                );
            };
            camera.is_active = true;
            frames.push(CompositionOutputFrame {
                output: request.output,
                frame,
            });
        }
        {
            let _prepare_span =
                tracing::trace_span!(target: crate::PROFILE_TARGET, "weld_prepare_dmabuf_imports")
                    .entered();
            if let Some(importer) = &mut self.dmabuf_importer {
                importer.prepare_render(&mut self.app)?;
            }
        }
        let installed_images = self
            .dmabuf_importer
            .as_ref()
            .map(DmabufImporter::installed_image_ids)
            .unwrap_or_default();
        publish_surface_bindings(&mut self.app, installed_images);
        {
            let _bevy_render_span =
                tracing::trace_span!(target: crate::PROFILE_TARGET, "bevy_render_composition")
                    .entered();
            render_composition_app(&mut self.app);
        }
        {
            let _finish_span =
                tracing::trace_span!(target: crate::PROFILE_TARGET, "weld_finish_dmabuf_imports")
                    .entered();
            if let Some(importer) = &mut self.dmabuf_importer {
                importer.finish_render(&mut self.app)?;
            }
        }
        Ok(())
    }

    pub fn should_exit(&self) -> bool {
        self.app.should_exit().is_some()
    }

    pub fn update_output_topology(&mut self, configurations: &[OutputConfiguration]) {
        crate::input::update_output_configurations(self.app.world_mut(), configurations);
        for configuration in configurations {
            let Some(output) = self.outputs.get_mut(&configuration.id()) else {
                tracing::warn!(
                    output = ?configuration.id(),
                    "dynamic output addition requires restarting Weld"
                );
                continue;
            };
            if output.owned_target.target.extent() != configuration.extent() {
                output.owned_target = OwnedCompositionTarget::new(
                    &self.device,
                    configuration.extent(),
                    output.owned_target.target.format(),
                );
            }
            output.configuration = *configuration;
            insert_manual_view(
                &mut self.app,
                output.view,
                &output.owned_target.target,
                configuration.scale().value(),
            );
            if let Some(mut geometry) = self
                .app
                .world_mut()
                .get_mut::<OutputGeometry>(output.entity)
            {
                geometry.set_if_neq(OutputGeometry::new(
                    configuration.extent(),
                    configuration.scale().value(),
                ));
            }
            if let Some(mut position) = self
                .app
                .world_mut()
                .get_mut::<OutputPosition>(output.entity)
            {
                position.set_if_neq(OutputPosition(Vec2::new(
                    configuration.position().x,
                    configuration.position().y,
                )));
            }
            if let Some(mut placement) = self
                .app
                .world_mut()
                .get_mut::<OutputPlacement>(output.entity)
            {
                placement.set_if_neq(OutputPlacement::from_configuration(*configuration));
            }
        }
    }

    pub fn enqueue_client_event(&mut self, event: ClientSurfaceEvent) -> CompositionDemand {
        let demand = self.surface_demand.classify(&event);
        let ClientSurfaceEvent { surface, kind } = event;
        match kind {
            ClientSurfaceEventKind::Commit(commit) => {
                let _ingress_span = tracing::trace_span!(
                    target: crate::PROFILE_TARGET,
                    "weld_surface_snapshot_ingress"
                )
                .entered();
                let snapshot = self.prepare_surface_commit(surface, commit);
                enqueue_surface_event(
                    self.app.world_mut(),
                    HostSurfaceEvent {
                        surface,
                        kind: HostSurfaceEventKind::Commit(snapshot),
                    },
                );
            }
            ClientSurfaceEventKind::Destroyed => {
                if let Some(importer) = &mut self.dmabuf_importer {
                    importer.remove_surface(surface);
                }
                enqueue_surface_event(
                    self.app.world_mut(),
                    HostSurfaceEvent {
                        surface,
                        kind: HostSurfaceEventKind::Destroyed,
                    },
                );
            }
            ClientSurfaceEventKind::Role(role) => enqueue_surface_event(
                self.app.world_mut(),
                HostSurfaceEvent {
                    surface,
                    kind: HostSurfaceEventKind::Role(role),
                },
            ),
            ClientSurfaceEventKind::Interaction(request) => enqueue_surface_event(
                self.app.world_mut(),
                HostSurfaceEvent {
                    surface,
                    kind: HostSurfaceEventKind::Interaction(request),
                },
            ),
        }
        demand
    }

    fn prepare_surface_commit(
        &mut self,
        surface: crate::surface::SurfaceId,
        commit: weld_client::ClientSurfaceCommit,
    ) -> SurfaceTreeSnapshot {
        let weld_client::ClientSurfaceCommit {
            revision: _,
            mapped,
            root,
            window_geometry,
            overlays,
            inputs,
            buffers,
        } = commit;
        let retained = buffers
            .iter()
            .filter(|buffer| !matches!(buffer.change, SurfaceBufferChange::Removed))
            .map(|buffer| buffer.layer)
            .collect();
        if let Some(importer) = &mut self.dmabuf_importer {
            importer.retain_surface_layers(surface, &retained);
        }
        let buffers = buffers
            .into_iter()
            .filter_map(|buffer| {
                let metadata = buffer.change.metadata()?;
                let content = match buffer.change {
                    SurfaceBufferChange::Retained { .. } => SurfaceBufferContent::Retained,
                    SurfaceBufferChange::Removed => return None,
                    SurfaceBufferChange::Replaced { buffer: lease, .. } => {
                        match resolve_client_buffer_access(
                            &self.client_importers,
                            surface,
                            &lease,
                        ) {
                            ClientBufferAccessResolution::Direct(access)
                                if matches!(access.as_ref(), DirectClientBufferAccess::Shm(_)) =>
                            {
                                if let Some(importer) = &mut self.dmabuf_importer {
                                    importer.remove_layer(surface, buffer.layer);
                                }
                                drop(lease);
                                let pixels = match std::rc::Rc::try_unwrap(access) {
                                    Ok(DirectClientBufferAccess::Shm(buffer)) => buffer.bgra_pixels,
                                    Ok(DirectClientBufferAccess::Dmabuf(_)) => return None,
                                    Err(access) => match access.as_ref() {
                                        DirectClientBufferAccess::Shm(buffer) => buffer.bgra_pixels.clone(),
                                        DirectClientBufferAccess::Dmabuf(_) => return None,
                                    },
                                };
                                SurfaceBufferContent::Pixels(pixels)
                            }
                            ClientBufferAccessResolution::Direct(access)
                                if matches!(access.as_ref(), DirectClientBufferAccess::Dmabuf(_)) =>
                            {
                                let imported = if let Some(importer) = &mut self.dmabuf_importer {
                                    importer
                                        .import(
                                            &mut self.app,
                                            surface,
                                            buffer.layer,
                                            lease,
                                            metadata.opaque,
                                        )
                                        .map_err(|error| {
                                            tracing::warn!(
                                                %error,
                                                ?surface,
                                                layer = ?buffer.layer,
                                                "failed to import a committed DMA-BUF"
                                            );
                                        })
                                        .ok()
                                } else {
                                    tracing::warn!(?surface, layer = ?buffer.layer, "received a DMA-BUF without an importer");
                                    None
                                };
                                imported
                                    .map(SurfaceBufferContent::RenderImage)
                                    .unwrap_or(SurfaceBufferContent::Retained)
                            }
                            ClientBufferAccessResolution::Unregistered => {
                                tracing::warn!(?surface, layer = ?buffer.layer, "dropped a client-buffer lease without a registered importer");
                                SurfaceBufferContent::Retained
                            }
                            ClientBufferAccessResolution::SourceMismatch => {
                                tracing::warn!(?surface, layer = ?buffer.layer, "dropped a client-buffer lease from another source");
                                SurfaceBufferContent::Retained
                            }
                            ClientBufferAccessResolution::Unsupported
                            | ClientBufferAccessResolution::Direct(_) => {
                                tracing::warn!(?surface, layer = ?buffer.layer, "client-buffer lease carried unsupported access");
                                SurfaceBufferContent::Retained
                            }
                        }
                    }
                };
                Some(SurfaceBufferUpdate {
                    layer: buffer.layer,
                    width: metadata.extent.width,
                    height: metadata.extent.height,
                    content,
                    opaque: metadata.opaque,
                })
            })
            .collect();
        SurfaceTreeSnapshot {
            client_mapped: mapped,
            root: root.map(app_layer_placement),
            window_geometry: window_geometry.map(app_window_geometry),
            overlays: overlays.into_iter().map(app_layer_placement).collect(),
            inputs: inputs.into_iter().map(app_input_placement).collect(),
            buffers,
        }
    }

    pub fn enqueue_input_event(&mut self, event: RawSeatEvent) -> bool {
        self.pending_input.enqueue(self.app.world_mut(), event)
    }

    pub fn take_pointer_route_updates(&mut self) -> Vec<weld_client::ClientPointerRouteUpdate> {
        take_input_effects(self.app.world_mut())
    }

    pub fn take_cursor_update(&mut self) -> weld_core::cursor::CursorHostUpdate {
        take_cursor_update(self.app.world(), &mut self.cursor)
    }

    pub fn take_host_commands(&mut self) -> Vec<HostCommand> {
        take_host_commands(self.app.world_mut())
    }

    pub fn take_virtual_terminal_switch_request(&mut self) -> Option<i32> {
        take_virtual_terminal_switch_request(self.app.world_mut())
    }

    pub fn take_client_requests(&mut self) -> Vec<ClientRequest> {
        let mut requests = Vec::new();
        for action in take_surface_actions(self.app.world_mut()) {
            requests.push(client_request(action));
        }
        requests
    }

    pub fn take_adapter_commands(&mut self) -> Vec<weld_client::ClientAdapterCommandEnvelope> {
        self.app
            .world_mut()
            .get_resource_mut::<ClientAdapterCommandQueue>()
            .map(|mut commands| commands.take().into_iter().collect())
            .unwrap_or_default()
    }

    pub fn complete_dmabuf_uses(&mut self, uses: &[weld_client::ClientBufferUseId]) {
        if let Some(importer) = &mut self.dmabuf_importer {
            importer.complete_gpu_uses(uses);
        }
    }

    pub fn has_surface_frame(&self) -> bool {
        has_surface_frame(self.app.world())
    }

    pub fn take_capture_request(&mut self) -> Option<CaptureRequest> {
        take_capture_request(self.app.world_mut())
    }

    pub fn complete_capture(&mut self, request_id: u64, result: Result<(), String>) {
        complete_capture(self.app.world_mut(), request_id, result);
    }

    /// Waits until all submitted render work has completed.
    ///
    /// Normal compositor operation synchronizes through presentation and
    /// client-buffer lifetimes. Headless benchmarks use this explicit wait to
    /// distinguish CPU submission cost from end-to-end GPU completion.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn wait_for_gpu_for_benchmark(&self) -> Result<()> {
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(std::time::Duration::from_secs(5)),
            })
            .context("headless render benchmark GPU wait timed out")?;
        Ok(())
    }
}

fn output_entities(world: &mut World) -> Result<HashMap<OutputId, Entity>> {
    let mut query = world.query::<(Entity, &WeldOutput, &OutputGeometry)>();
    let mut entities = HashMap::new();
    for (entity, output, _) in query.iter(world) {
        if entities.insert(output.id, entity).is_some() {
            bail!("Weld application contains duplicate output {:?}", output.id);
        }
    }
    if entities.is_empty() {
        bail!("WeldAppPlugin did not create any outputs");
    }
    Ok(entities)
}

fn spawn_compositor_camera(
    world: &mut World,
    output: Entity,
    view: ManualTextureViewHandle,
    primary: bool,
) -> Entity {
    let mut camera = world.spawn((
        Camera2d,
        Camera {
            // Direct GBM scanout has no later full-output clear. Keeping this
            // explicit also guarantees the cursor pass loads initialized pixels.
            clear_color: ClearColorConfig::Custom(Color::linear_rgba(0.025, 0.032, 0.045, 1.0)),
            ..Default::default()
        },
        RenderTarget::TextureView(view),
        // Bevy UI shaders emit linear RGB. The manual sRGB target performs
        // the transfer encoding when those values are written.
        CompositingSpace::Linear,
        RendersOutput(output),
    ));
    if primary {
        camera.insert(IsDefaultUiCamera);
    }
    camera.id()
}

impl CompositionHost for AppShell {
    fn enqueue_client_event(&mut self, event: ClientSurfaceEvent) -> CompositionDemand {
        AppShell::enqueue_client_event(self, event)
    }

    fn enqueue_input_event(&mut self, event: RawSeatEvent) -> bool {
        AppShell::enqueue_input_event(self, event)
    }

    fn advance_main(&mut self, input_time: u32) -> bool {
        AppShell::advance_main(self, input_time)
    }

    fn service_remote_debug(&mut self) {
        AppShell::service_remote_debug(self);
    }

    fn render_outputs(
        &mut self,
        requests: &[CompositionOutputRequest],
        frames: &mut Vec<CompositionOutputFrame>,
    ) -> Result<()> {
        AppShell::render_outputs(self, requests, frames)
    }

    fn update_output_topology(&mut self, outputs: &[OutputConfiguration]) {
        AppShell::update_output_topology(self, outputs);
    }

    fn should_exit(&self) -> bool {
        AppShell::should_exit(self)
    }

    fn take_pointer_route_updates(&mut self) -> Vec<weld_client::ClientPointerRouteUpdate> {
        AppShell::take_pointer_route_updates(self)
    }

    fn take_cursor_update(&mut self) -> weld_core::cursor::CursorHostUpdate {
        AppShell::take_cursor_update(self)
    }

    fn take_host_commands(&mut self) -> Vec<HostCommand> {
        AppShell::take_host_commands(self)
    }

    fn take_virtual_terminal_switch_request(&mut self) -> Option<i32> {
        AppShell::take_virtual_terminal_switch_request(self)
    }

    fn take_client_requests(&mut self) -> Vec<ClientRequest> {
        AppShell::take_client_requests(self)
    }

    fn take_adapter_commands(&mut self) -> Vec<weld_client::ClientAdapterCommandEnvelope> {
        AppShell::take_adapter_commands(self)
    }

    fn complete_dmabuf_uses(&mut self, uses: &[weld_client::ClientBufferUseId]) {
        AppShell::complete_dmabuf_uses(self, uses);
    }

    fn has_surface_frame(&self) -> bool {
        AppShell::has_surface_frame(self)
    }

    fn take_capture_request(&mut self) -> Option<CaptureRequest> {
        AppShell::take_capture_request(self)
    }

    fn complete_capture(&mut self, request_id: u64, result: Result<(), String>) {
        AppShell::complete_capture(self, request_id, result);
    }
}

fn app_content_view(view: weld_core::surface::SurfaceContentView) -> SurfaceContentView {
    SurfaceContentView {
        source_x: view.source_x,
        source_y: view.source_y,
        source_width: view.source_width,
        source_height: view.source_height,
        logical_width: view.logical_width,
        logical_height: view.logical_height,
    }
}

fn app_layer_placement(
    placement: weld_core::surface::SurfaceLayerPlacement,
) -> SurfaceLayerPlacement {
    SurfaceLayerPlacement {
        layer: placement.layer,
        position: bevy::math::Vec2::new(placement.position.x, placement.position.y),
        view: app_content_view(placement.view),
    }
}

fn app_window_geometry(
    geometry: weld_core::surface::SurfaceWindowGeometry,
) -> SurfaceWindowGeometry {
    SurfaceWindowGeometry {
        origin: bevy::math::Vec2::new(geometry.origin.x, geometry.origin.y),
        view: app_content_view(geometry.view),
    }
}

fn app_input_placement(
    placement: weld_core::surface::SurfaceInputPlacement,
) -> SurfaceInputPlacement {
    SurfaceInputPlacement {
        layer: placement.layer,
        position: bevy::math::Vec2::new(placement.position.x, placement.position.y),
        regions: placement
            .regions
            .into_iter()
            .map(|region| SurfaceInputRect {
                position: bevy::math::Vec2::new(region.position.x, region.position.y),
                size: bevy::math::Vec2::new(region.size.width, region.size.height),
            })
            .collect(),
    }
}

fn client_request(action: SurfaceAction) -> ClientRequest {
    match action {
        SurfaceAction::Close { surface } => ClientRequest::Surface(ClientSurfaceRequest {
            surface,
            kind: ClientSurfaceRequestKind::Close,
        }),
        SurfaceAction::Focus {
            surface: Some(surface),
        } => ClientRequest::Focus(ClientFocusRequest {
            source: surface.source(),
            surface: Some(surface),
        }),
        SurfaceAction::Focus { surface: None } => ClientRequest::ClearFocus,
        SurfaceAction::Resize {
            surface,
            logical_size,
        } => ClientRequest::Surface(ClientSurfaceRequest {
            surface,
            kind: ClientSurfaceRequestKind::Configure {
                logical_size: Extent::new(logical_size.x, logical_size.y),
            },
        }),
        SurfaceAction::SetOutputs {
            surface,
            outputs,
            preferred,
        } => ClientRequest::Surface(ClientSurfaceRequest {
            surface,
            kind: ClientSurfaceRequestKind::SetOutputs {
                outputs: outputs
                    .into_iter()
                    .map(|output| ClientOutputId::new(output.raw()))
                    .collect(),
                preferred: preferred.map(|output| ClientOutputId::new(output.raw())),
            },
        }),
    }
}

fn advance_main_app(app: &mut App, redraw_requests: &mut RedrawRequests) -> bool {
    app.main_mut().run_default_schedule();
    let Some(messages) = app.world().get_resource::<Messages<RequestRedraw>>() else {
        return false;
    };
    redraw_requests.take(messages)
}

fn disconnect_render_time(app: &mut App) -> Result<()> {
    // Weld advances the non-pipelined main world independently of RenderApp,
    // so Automatic time must use its documented main-world clock fallback.
    // Dropping the sole receiver is load-bearing: Bevy 0.19.1's render-side
    // send_time explicitly ignores Disconnected. Re-verify both assumptions
    // when updating Bevy.
    let receiver = app
        .world_mut()
        .remove_resource::<TimeReceiver>()
        .context("Bevy RenderPlugin did not register its main-world TimeReceiver")?;
    drop(receiver);
    Ok(())
}

fn render_composition_app(app: &mut App) {
    app.update_sub_app_by_label(RenderApp);
    app.world_mut().clear_trackers();
}

struct RedrawRequests(MessageCursor<RequestRedraw>);

impl RedrawRequests {
    fn new(messages: &Messages<RequestRedraw>) -> Self {
        Self(messages.get_cursor_current())
    }

    fn take(&mut self, messages: &Messages<RequestRedraw>) -> bool {
        let pending = self.0.len(messages) > 0;
        self.0.clear(messages);
        pending
    }
}

fn insert_manual_view(
    app: &mut App,
    handle: ManualTextureViewHandle,
    target: &CompositionTargetView,
    scale_factor: f64,
) {
    app.world_mut().resource_mut::<ManualTextureViews>().insert(
        handle,
        ManualTextureView {
            texture_view: target.view().clone().into(),
            size: UVec2::new(target.extent().width, target.extent().height),
            view_format: target.format(),
            scale_factor: scale_factor as f32,
        },
    );
}

fn disable_ui_rounding_on_roots(world: &mut World) {
    let roots = {
        let mut query =
            world.query_filtered::<Entity, (With<Node>, Without<ChildOf>, Without<LayoutConfig>)>();
        query.iter(world).collect::<Vec<_>>()
    };
    for root in roots {
        world.entity_mut(root).insert(LayoutConfig {
            use_rounding: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use bevy::{
        app::{HierarchyPropagatePlugin, PostUpdate, PropagateSet, SubApp, Update},
        ecs::{
            message::MessageWriter,
            resource::Resource,
            schedule::{Schedule, ScheduleLabel},
            system::ResMut,
        },
        render::RenderApp,
        time::{Real, Time, TimePlugin, TimeReceiver, create_time_channels},
        ui::{ComputedUiTargetCamera, Node, UiScale, update::propagate_ui_target_cameras},
        window::{ExitCondition, RequestRedraw, WindowPlugin},
    };

    use super::{
        App, AppShell, ClientBufferAccessResolution, CompositionTargetContract,
        ManualTextureViewHandle, Messages, OutputGeometry, PRIMARY_OUTPUT_ID, RedrawRequests,
        SurfaceCompositionDemand, UVec2, WeldOutput, advance_main_app, disconnect_render_time,
        render_composition_app, resolve_client_buffer_access, spawn_compositor_camera,
        validate_external_target,
    };
    use weld_client::{
        ClientCommitRevision, ClientSurfaceCommit, ClientSurfaceEvent, ClientSurfaceEventKind,
    };
    use weld_core::{
        CompositionDemand,
        surface::{Extent, SurfaceId},
    };

    #[cfg(feature = "test-support")]
    use bevy::ui::UiTargetCamera;
    #[cfg(feature = "test-support")]
    use weld_core::{
        OutputConfiguration, OutputId, OutputScale,
        host::{CompositionDestination, CompositionOutputRequest},
        surface::{
            LogicalPoint, SurfaceContentView, SurfaceLayerId, SurfaceLayerPlacement,
            SurfaceWindowGeometry, WindowDecoration,
        },
    };

    #[cfg(feature = "test-support")]
    use crate::surface::{SurfaceNode, SurfaceView};

    #[derive(Resource, Default)]
    struct RenderCount(u32);

    fn request_redraw(mut requests: MessageWriter<RequestRedraw>) {
        requests.write(RequestRedraw);
        requests.write(RequestRedraw);
    }

    fn count_render(mut count: ResMut<RenderCount>) {
        count.0 += 1;
    }

    #[test]
    fn external_target_contract_rejects_extent_and_format_mismatches() {
        let expected = CompositionTargetContract {
            extent: Extent::new(1920, 1080),
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
        };
        assert!(
            validate_external_target(PRIMARY_OUTPUT_ID, expected, expected).is_ok(),
            "matching external targets must remain usable"
        );

        let wrong_extent = CompositionTargetContract {
            extent: Extent::new(1280, 720),
            ..expected
        };
        let extent_error = validate_external_target(PRIMARY_OUTPUT_ID, expected, wrong_extent)
            .expect_err("mismatched extent must be rejected");
        assert!(extent_error.to_string().contains("has extent"));

        let wrong_format = CompositionTargetContract {
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            ..expected
        };
        let format_error = validate_external_target(PRIMARY_OUTPUT_ID, expected, wrong_format)
            .expect_err("mismatched format must be rejected");
        assert!(format_error.to_string().contains("has format"));
    }

    #[test]
    fn client_buffer_access_rejects_unknown_sources_and_payloads_before_ecs() {
        use std::{cell::Cell, collections::HashSet, rc::Rc};

        use weld_client::{
            ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId,
            ClientSourceId,
        };

        let surface = SurfaceId::for_test(1);
        let registered = HashSet::from([surface.source()]);
        let completed = Rc::new(Cell::new(0));
        let lease = |source: ClientSourceId, payload: Rc<String>| {
            let completed = completed.clone();
            ClientBufferLease::new(
                ClientBufferId::new(source, 1),
                ClientBufferUseId::new(source, 1),
                ClientBufferMetadata::new(Extent::new(1, 1), false),
                payload,
                move |_| completed.set(completed.get() + 1),
            )
            .expect("matching lease source")
        };

        let unsupported = lease(surface.source(), Rc::new(String::from("unsupported")));
        assert!(matches!(
            resolve_client_buffer_access(&registered, surface, &unsupported),
            ClientBufferAccessResolution::Unsupported
        ));
        drop(unsupported);

        let foreign_source = ClientSourceId::new(9);
        let foreign = lease(foreign_source, Rc::new(String::from("foreign")));
        assert!(matches!(
            resolve_client_buffer_access(&registered, surface, &foreign),
            ClientBufferAccessResolution::SourceMismatch
        ));
        drop(foreign);

        let foreign_surface =
            weld_client::ClientSurfaceId::new(weld_client::ClientId::new(foreign_source, 1), 1);
        let unregistered = lease(foreign_source, Rc::new(String::from("unregistered")));
        assert!(matches!(
            resolve_client_buffer_access(&registered, foreign_surface, &unregistered),
            ClientBufferAccessResolution::Unregistered
        ));
        drop(unregistered);
        assert_eq!(completed.get(), 3);

        fn assert_send<T: Send>() {}
        assert_send::<crate::surface::HostSurfaceEvent>();
    }

    fn test_app() -> (App, RedrawRequests) {
        let mut app = App::new();
        app.add_plugins(WindowPlugin {
            primary_window: None,
            exit_condition: ExitCondition::DontExit,
            ..Default::default()
        })
        .add_systems(Update, request_redraw);
        let requests = RedrawRequests::new(app.world().resource::<Messages<RequestRedraw>>());
        (app, requests)
    }

    fn snapshot_event(surface: SurfaceId, client_mapped: bool) -> ClientSurfaceEvent {
        ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Commit(ClientSurfaceCommit {
                revision: ClientCommitRevision::new(1),
                mapped: client_mapped,
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: Vec::new(),
            }),
        }
    }

    #[test]
    fn only_the_first_snapshot_of_a_mapping_requests_settling() {
        let surface = SurfaceId::for_test(1);
        let mut demand = SurfaceCompositionDemand::default();

        assert_eq!(
            demand.classify(&snapshot_event(surface, true)),
            CompositionDemand::Settle
        );
        assert_eq!(
            demand.classify(&snapshot_event(surface, true)),
            CompositionDemand::Ordinary
        );
    }

    #[test]
    fn an_unmapped_surface_settles_again_when_it_is_remapped() {
        let surface = SurfaceId::for_test(1);
        let mut demand = SurfaceCompositionDemand::default();

        assert_eq!(
            demand.classify(&snapshot_event(surface, true)),
            CompositionDemand::Settle
        );
        assert_eq!(
            demand.classify(&snapshot_event(surface, false)),
            CompositionDemand::Settle
        );
        assert_eq!(
            demand.classify(&snapshot_event(surface, true)),
            CompositionDemand::Settle
        );
    }

    #[test]
    fn consumes_redraw_requests_once_after_each_app_update() {
        let (mut app, mut requests) = test_app();

        // The full Weld app may retain messages longer through TimePlugin; this
        // minimal app exercises the shorter default per-update retention.
        assert!(advance_main_app(&mut app, &mut requests));
        assert!(!requests.take(app.world().resource::<Messages<RequestRedraw>>()));

        assert!(advance_main_app(&mut app, &mut requests));
    }

    #[test]
    fn main_advance_skips_render_app_until_composition() {
        let (mut app, mut requests) = test_app();
        let mut render_app = SubApp::new();
        render_app
            .world_mut()
            .insert_resource(RenderCount::default());
        let mut render_schedule = Schedule::new(Update);
        render_schedule.add_systems(count_render);
        render_app.world_mut().add_schedule(render_schedule);
        render_app.update_schedule = Some(Update.intern());
        app.insert_sub_app(RenderApp, render_app);

        advance_main_app(&mut app, &mut requests);
        assert_eq!(
            app.sub_app(RenderApp).world().resource::<RenderCount>().0,
            0
        );

        render_composition_app(&mut app);
        assert_eq!(
            app.sub_app(RenderApp).world().resource::<RenderCount>().0,
            1
        );
    }

    #[test]
    fn disconnected_render_time_uses_the_main_world_clock_fallback() {
        let mut app = App::new();
        app.add_plugins(TimePlugin);
        let (_sender, receiver) = create_time_channels();
        app.insert_resource(receiver);

        disconnect_render_time(&mut app).expect("test receiver should be removed");
        assert!(!app.world().contains_resource::<TimeReceiver>());
        app.update();
        let first_elapsed = app.world().resource::<Time<Real>>().elapsed();
        app.update();
        let second_elapsed = app.world().resource::<Time<Real>>().elapsed();
        assert!(second_elapsed > first_elapsed);
    }

    #[test]
    fn untargeted_ui_roots_use_the_compositor_camera() {
        let mut app = App::new();
        app.init_resource::<UiScale>()
            .add_plugins(HierarchyPropagatePlugin::<ComputedUiTargetCamera>::new(
                PostUpdate,
            ))
            .configure_sets(
                PostUpdate,
                PropagateSet::<ComputedUiTargetCamera>::default(),
            )
            .add_systems(Update, propagate_ui_target_cameras);

        let output = app
            .world_mut()
            .spawn((
                WeldOutput {
                    id: PRIMARY_OUTPUT_ID,
                },
                OutputGeometry::from_physical(UVec2::ONE, 1.0),
            ))
            .id();
        let camera =
            spawn_compositor_camera(app.world_mut(), output, ManualTextureViewHandle(1), true);
        let root = app.world_mut().spawn(Node::default()).id();
        app.update();

        assert_eq!(
            app.world()
                .get::<ComputedUiTargetCamera>(root)
                .and_then(ComputedUiTargetCamera::get),
            Some(camera),
        );
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn sequential_output_passes_preserve_surface_material_updates() {
        let first = OutputId::new(1);
        let second = OutputId::new(2);
        let configurations = vec![
            diagnostic_output(first, LogicalPoint::ZERO, true, 0.0),
            diagnostic_output(second, LogicalPoint::new(64.0, 0.0), false, 20.0),
        ];
        let (mut shell, _, device, queue) =
            match crate::benchmark::rendering_shell_with_outputs(configurations, |_| {}) {
                Ok(shell) => shell,
                Err(error) if error.to_string().contains("no Vulkan adapter is available") => {
                    eprintln!("skipped Vulkan surface composition diagnostic: {error}");
                    return;
                }
                Err(error) => panic!("headless two-output shell should initialize: {error}"),
            };
        let first_surface = SurfaceId::for_test(1);
        let second_surface = SurfaceId::for_test(2);
        install_diagnostic_surface(&mut shell, first_surface, [0, 0, 255, 255]);
        install_diagnostic_surface(&mut shell, second_surface, [0, 255, 0, 255]);
        let first_camera = shell.outputs[&first].camera;
        let second_camera = shell.outputs[&second].camera;
        shell.app.world_mut().spawn((
            SurfaceNode {
                surface: first_surface,
                view: SurfaceView::FullSurface,
            },
            UiTargetCamera(first_camera),
        ));
        shell.app.world_mut().spawn((
            SurfaceNode {
                surface: second_surface,
                view: SurfaceView::FullSurface,
            },
            UiTargetCamera(second_camera),
        ));

        for time in 1..=5 {
            shell.advance_main(time);
            let mut frames = Vec::new();
            shell
                .render_outputs(&[owned_request(first), owned_request(second)], &mut frames)
                .expect("initial surface assets should settle");
        }
        assert_surface_pixel(
            render_owned_output(&mut shell, &device, &queue, first),
            [255, 0, 0, 255],
        );
        assert_surface_pixel(
            render_owned_output(&mut shell, &device, &queue, second),
            [0, 255, 0, 255],
        );

        update_diagnostic_surface(&mut shell, first_surface, [255, 0, 0, 255]);
        update_diagnostic_surface(&mut shell, second_surface, [0, 255, 255, 255]);
        shell.advance_main(6);
        assert_surface_pixel(
            render_owned_output(&mut shell, &device, &queue, first),
            [0, 0, 255, 255],
        );
        assert_surface_pixel(
            render_owned_output(&mut shell, &device, &queue, second),
            [255, 255, 0, 255],
        );

        update_diagnostic_surface(&mut shell, first_surface, [255, 0, 255, 255]);
        update_diagnostic_surface(&mut shell, second_surface, [255, 255, 0, 255]);
        shell.advance_main(7);
        assert_surface_pixel(
            render_owned_output(&mut shell, &device, &queue, second),
            [0, 255, 255, 255],
        );
        assert_surface_pixel(
            render_owned_output(&mut shell, &device, &queue, first),
            [255, 0, 255, 255],
        );
    }

    #[cfg(feature = "test-support")]
    fn diagnostic_output(
        id: OutputId,
        position: LogicalPoint,
        primary: bool,
        physical_x: f64,
    ) -> OutputConfiguration {
        OutputConfiguration::new(
            id,
            Extent::new(64, 64),
            OutputScale::default(),
            position,
            primary,
            None,
        )
        .and_then(|output| output.with_footprint_position(physical_x, 0.0))
        .expect("diagnostic output should be valid")
    }

    #[cfg(feature = "test-support")]
    fn install_diagnostic_surface(shell: &mut AppShell, surface: SurfaceId, bgra: [u8; 4]) {
        shell.enqueue_client_event(ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Role(weld_client::ClientSurfaceRole::Toplevel(
                weld_client::ToplevelState {
                    parent: None,
                    decoration: WindowDecoration::ClientSide,
                },
            )),
        });
        update_diagnostic_surface(shell, surface, bgra);
    }

    #[cfg(feature = "test-support")]
    fn update_diagnostic_surface(shell: &mut AppShell, surface: SurfaceId, bgra: [u8; 4]) {
        const SIZE: u32 = 32;
        let view = SurfaceContentView {
            source_x: 0.0,
            source_y: 0.0,
            source_width: SIZE as f32,
            source_height: SIZE as f32,
            logical_width: SIZE as f32,
            logical_height: SIZE as f32,
        };
        let layer = SurfaceLayerId::new(1);
        let pixels = bgra
            .into_iter()
            .cycle()
            .take((SIZE * SIZE * 4) as usize)
            .collect();
        let metadata = weld_client::ClientBufferMetadata::new(Extent::new(SIZE, SIZE), true);
        let lease = weld_client::ClientBufferLease::new(
            weld_client::ClientBufferId::new(weld_core::WAYLAND_CLIENT_SOURCE, 1),
            weld_client::ClientBufferUseId::new(weld_core::WAYLAND_CLIENT_SOURCE, 1),
            metadata,
            std::rc::Rc::new(weld_core::dmabuf::DirectClientBufferAccess::Shm(
                weld_core::dmabuf::WaylandShmBuffer {
                    bgra_pixels: pixels,
                },
            )),
            |_| {},
        )
        .expect("matching diagnostic buffer source");
        shell.enqueue_client_event(ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Commit(ClientSurfaceCommit {
                revision: ClientCommitRevision::new(1),
                mapped: true,
                root: Some(SurfaceLayerPlacement {
                    layer,
                    position: LogicalPoint::ZERO,
                    view,
                }),
                window_geometry: Some(SurfaceWindowGeometry {
                    origin: LogicalPoint::ZERO,
                    view,
                }),
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: vec![weld_client::SurfaceBufferUpdate {
                    layer,
                    change: weld_client::SurfaceBufferChange::Replaced {
                        metadata,
                        buffer: lease,
                    },
                }],
            }),
        });
    }

    #[cfg(feature = "test-support")]
    fn owned_request(output: OutputId) -> CompositionOutputRequest {
        CompositionOutputRequest {
            output,
            destination: CompositionDestination::Owned,
        }
    }

    #[cfg(feature = "test-support")]
    fn render_owned_output(
        shell: &mut AppShell,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        output: OutputId,
    ) -> Vec<u8> {
        let mut frames = Vec::new();
        shell
            .render_outputs(&[owned_request(output)], &mut frames)
            .expect("diagnostic output should render");
        let frame = frames
            .first()
            .expect("diagnostic render should return one frame");
        weld_core::renderer::read_owned_frame_rgba(device, queue, &frame.frame)
            .expect("diagnostic output should be readable")
    }

    #[cfg(feature = "test-support")]
    fn assert_surface_pixel(pixels: Vec<u8>, expected: [u8; 4]) {
        const TARGET_WIDTH: usize = 64;
        let offset = (16 * TARGET_WIDTH + 16) * 4;
        assert_eq!(&pixels[offset..offset + 4], expected.as_slice());
    }
}
