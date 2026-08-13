//! Bevy-owned compositor scene rendered into a Weld-owned wgpu texture.

use std::{collections::HashSet, sync::Arc};

use anyhow::{Context, Result};
use bevy::{
    app::{App, Plugin, PluginGroup, PostUpdate, TerminalCtrlCHandlerPlugin},
    camera::{
        Camera, Camera2d, ClearColorConfig, CompositingSpace, ManualTextureViewHandle,
        NormalizedRenderTarget, RenderTarget,
    },
    ecs::{
        message::{MessageCursor, Messages},
        schedule::IntoScheduleConfigs,
        world::World,
    },
    log::LogPlugin,
    math::UVec2,
    prelude::{ChildOf, Color, DefaultPlugins, Entity, LayoutConfig, Node, With, Without},
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

use crate::composition::set_composition_advance;
use crate::debug::{complete_capture, take_capture_request};
use crate::dmabuf::DmabufImporter;
use crate::input::{
    InputBridgePlugin, enqueue_raw_input, projected_pointer_position, set_input_update_time,
    take_host_commands, take_input_effects, take_virtual_terminal_switch_request,
};
use crate::output::OutputGeometry;
use crate::surface::{
    AppPopup, HostSurfaceEvent, HostSurfaceEventKind, SurfaceAction, SurfaceBufferContent,
    SurfaceBufferUpdate, SurfaceContentView, SurfaceInputPlacement, SurfaceInputRect,
    SurfaceLayerPlacement, SurfaceTreeSnapshot, SurfaceWindowGeometry, WindowDecoration,
    WindowInteractionRequestKind, WindowResizeEdge, enqueue_surface_event, has_surface_frame,
    take_surface_actions,
};
use weld_core::host::{CaptureRequest, RenderContext};
use weld_core::input::{InputPosition, RawSeatEvent, SeatInputEffect};
use weld_core::runtime::HostCommand;
use weld_core::server::{
    PendingSurfaceBufferContent, PendingSurfaceEvent, PendingSurfaceEventKind,
    PendingSurfaceTreeSnapshot,
};
use weld_core::surface::{Extent, SurfaceAction as CoreSurfaceAction};
use weld_core::{CompositionDemand, CompositionHost, dmabuf::DmabufContext};

const COMPOSITION_VIEW: ManualTextureViewHandle = ManualTextureViewHandle(1);
pub struct AppShell {
    app: App,
    redraw_requests: RedrawRequests,
    dmabuf_importer: Option<DmabufImporter>,
    dmabuf: DmabufContext,
    surface_demand: SurfaceCompositionDemand,
}

#[derive(Default)]
struct SurfaceCompositionDemand {
    mapped_surfaces: HashSet<crate::surface::SurfaceId>,
}

impl SurfaceCompositionDemand {
    fn classify(&mut self, event: &PendingSurfaceEvent) -> CompositionDemand {
        let surface = event.surface;
        match &event.kind {
            PendingSurfaceEventKind::TreeSnapshot(snapshot) if snapshot.client_mapped => {
                if self.mapped_surfaces.insert(surface) {
                    CompositionDemand::Settle
                } else {
                    CompositionDemand::Ordinary
                }
            }
            PendingSurfaceEventKind::TreeSnapshot(_) => {
                if self.mapped_surfaces.remove(&surface) {
                    CompositionDemand::Settle
                } else {
                    CompositionDemand::Ordinary
                }
            }
            PendingSurfaceEventKind::Destroyed => {
                self.mapped_surfaces.remove(&surface);
                CompositionDemand::Settle
            }
            PendingSurfaceEventKind::WindowInteraction(_) => CompositionDemand::Ordinary,
            PendingSurfaceEventKind::Created { .. } => {
                self.mapped_surfaces.remove(&surface);
                CompositionDemand::Settle
            }
            PendingSurfaceEventKind::DecorationChanged { .. }
            | PendingSurfaceEventKind::PopupConfigured(_) => CompositionDemand::Settle,
        }
    }
}

/// Installs Weld's application model without selecting a window policy.
pub(crate) struct WeldAppPlugin {
    extent: Extent,
    scale_factor: f64,
}

impl WeldAppPlugin {
    pub(crate) const fn new(extent: Extent, scale_factor: f64) -> Self {
        Self {
            extent,
            scale_factor,
        }
    }
}

impl Plugin for WeldAppPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            crate::composition::CompositionPlugin,
            crate::surface::SurfacePlugin,
            InputBridgePlugin::new(NormalizedRenderTarget::TextureView(COMPOSITION_VIEW)),
        ))
        .add_systems(
            PostUpdate,
            disable_ui_rounding_on_roots.before(UiSystems::Layout),
        )
        .insert_resource(UiScale(self.scale_factor as f32))
        .insert_resource(OutputGeometry::new(self.extent, self.scale_factor));
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
    pub fn new(mut app: App, context: RenderContext) -> Result<Self> {
        let _startup_span =
            tracing::trace_span!(target: crate::PROFILE_TARGET, "weld_app_shell_startup").entered();

        app.finish();
        app.cleanup();
        app.get_sub_app(RenderApp).context(
            "Bevy RenderPlugin did not create the non-pipelined RenderApp required by Weld",
        )?;
        disconnect_render_time(&mut app)?;

        insert_manual_view(
            &mut app,
            context.initial_target,
            context.extent.width,
            context.extent.height,
        );
        spawn_compositor_camera(app.world_mut());

        let redraw_requests = app
            .world()
            .get_resource::<Messages<RequestRedraw>>()
            .map(RedrawRequests::new)
            .context("Bevy WindowPlugin did not register redraw messages")?;
        let dmabuf_importer =
            DmabufImporter::new(&context.device, &context.queue, &context.dmabuf)?;

        Ok(Self {
            app,
            redraw_requests,
            dmabuf_importer,
            dmabuf: context.dmabuf,
            surface_demand: SurfaceCompositionDemand::default(),
        })
    }

    /// Advance Bevy policy and input without necessarily extracting or rendering.
    ///
    /// The host must call this exactly once for each logical advance. Client
    /// surface events are applied only when `composition_advance` is true, so
    /// their asset events remain paired with [`Self::render_composition`].
    /// Bevy time advances here at input/policy rate rather than render rate;
    /// systems must use time deltas instead of assuming one update per frame.
    pub fn advance_main(&mut self, input_time: u32, composition_advance: bool) -> bool {
        let _advance_span = if composition_advance {
            tracing::trace_span!(target: crate::PROFILE_TARGET, "weld_app_advance_composition")
                .entered()
        } else {
            tracing::trace_span!(target: crate::PROFILE_TARGET, "weld_app_advance_input").entered()
        };

        set_input_update_time(self.app.world_mut(), input_time);
        set_composition_advance(self.app.world_mut(), composition_advance);
        advance_main_app(&mut self.app, &mut self.redraw_requests)
    }

    /// Extract the preceding main-world advance and render Weld's composition.
    ///
    /// Construction pins Weld to Bevy's current non-pipelined [`RenderApp`].
    /// Main-world trackers are retained across input-only advances and cleared
    /// only after extraction has observed them.
    pub fn render_composition(&mut self, target: wgpu::TextureView, extent: Extent) -> Result<()> {
        let _composition_span =
            tracing::trace_span!(target: crate::PROFILE_TARGET, "weld_render_composition")
                .entered();

        insert_manual_view(&mut self.app, target, extent.width, extent.height);
        {
            let _prepare_span =
                tracing::trace_span!(target: crate::PROFILE_TARGET, "weld_prepare_dmabuf_imports")
                    .entered();
            if let Some(importer) = &mut self.dmabuf_importer {
                importer.prepare_render(&mut self.app)?;
            }
        }
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

    pub fn set_output_geometry(
        &mut self,
        target: wgpu::TextureView,
        extent: Extent,
        scale_factor: f64,
    ) {
        insert_manual_view(&mut self.app, target, extent.width, extent.height);
        self.app.world_mut().resource_mut::<UiScale>().0 = scale_factor as f32;
        self.app
            .world_mut()
            .resource_mut::<OutputGeometry>()
            .update(extent, scale_factor);
    }

    pub fn enqueue_surface_event(&mut self, event: PendingSurfaceEvent) -> CompositionDemand {
        let demand = self.surface_demand.classify(&event);
        let PendingSurfaceEvent { surface, kind } = event;
        match kind {
            PendingSurfaceEventKind::TreeSnapshot(snapshot) => {
                let _ingress_span = tracing::trace_span!(
                    target: crate::PROFILE_TARGET,
                    "weld_surface_snapshot_ingress"
                )
                .entered();
                let snapshot = self.prepare_surface_snapshot(surface, snapshot);
                enqueue_surface_event(
                    self.app.world_mut(),
                    HostSurfaceEvent {
                        surface,
                        kind: HostSurfaceEventKind::TreeSnapshot(snapshot),
                    },
                );
            }
            PendingSurfaceEventKind::Destroyed => {
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
            PendingSurfaceEventKind::Created { decoration } => {
                let _created_span = tracing::trace_span!(
                    target: crate::PROFILE_TARGET,
                    "weld_surface_created_ingress"
                )
                .entered();
                enqueue_surface_event(
                    self.app.world_mut(),
                    HostSurfaceEvent {
                        surface,
                        kind: HostSurfaceEventKind::Created {
                            decoration: app_decoration(decoration),
                        },
                    },
                );
            }
            PendingSurfaceEventKind::DecorationChanged { decoration } => enqueue_surface_event(
                self.app.world_mut(),
                HostSurfaceEvent {
                    surface,
                    kind: HostSurfaceEventKind::DecorationChanged {
                        decoration: app_decoration(decoration),
                    },
                },
            ),
            PendingSurfaceEventKind::PopupConfigured(popup) => enqueue_surface_event(
                self.app.world_mut(),
                HostSurfaceEvent {
                    surface,
                    kind: HostSurfaceEventKind::PopupConfigured(AppPopup {
                        owner: popup.owner,
                        position: bevy::math::Vec2::new(popup.position.x, popup.position.y),
                        stack_index: popup.stack_index,
                    }),
                },
            ),
            PendingSurfaceEventKind::WindowInteraction(request) => enqueue_surface_event(
                self.app.world_mut(),
                HostSurfaceEvent {
                    surface,
                    kind: HostSurfaceEventKind::WindowInteraction(app_interaction(request)),
                },
            ),
        }
        demand
    }

    fn prepare_surface_snapshot(
        &mut self,
        surface: crate::surface::SurfaceId,
        snapshot: PendingSurfaceTreeSnapshot,
    ) -> SurfaceTreeSnapshot {
        let PendingSurfaceTreeSnapshot {
            client_mapped,
            root,
            window_geometry,
            overlays,
            inputs,
            buffers,
        } = snapshot;
        let retained = buffers.iter().map(|buffer| buffer.layer).collect();
        if let Some(importer) = &mut self.dmabuf_importer {
            importer.retain_surface_layers(surface, &retained);
        }
        let buffers = buffers
            .into_iter()
            .map(|buffer| {
                let content = match buffer.content {
                    PendingSurfaceBufferContent::Retained => SurfaceBufferContent::Retained,
                    PendingSurfaceBufferContent::ShmPixels(pixels) => {
                        if let Some(importer) = &mut self.dmabuf_importer {
                            importer.remove_layer(surface, buffer.layer);
                        }
                        SurfaceBufferContent::Pixels(pixels)
                    }
                    PendingSurfaceBufferContent::ImportedDmabuf(frame) => {
                        let imported = if let Some(importer) = &mut self.dmabuf_importer {
                            importer
                                .import(
                                    &mut self.app,
                                    surface,
                                    buffer.layer,
                                    frame,
                                    buffer.opaque,
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
                            self.dmabuf.release_unrendered(frame);
                            tracing::warn!(?surface, layer = ?buffer.layer, "received a DMA-BUF without an importer");
                            None
                        };
                        imported
                            .map(SurfaceBufferContent::RenderImage)
                            .unwrap_or(SurfaceBufferContent::Retained)
                    }
                };
                SurfaceBufferUpdate {
                    layer: buffer.layer,
                    width: buffer.width,
                    height: buffer.height,
                    content,
                    opaque: buffer.opaque,
                }
            })
            .collect();
        SurfaceTreeSnapshot {
            client_mapped,
            root: root.map(app_layer_placement),
            window_geometry: window_geometry.map(app_window_geometry),
            overlays: overlays.into_iter().map(app_layer_placement).collect(),
            inputs: inputs.into_iter().map(app_input_placement).collect(),
            buffers,
        }
    }

    pub fn enqueue_input_event(&mut self, event: RawSeatEvent) {
        enqueue_raw_input(self.app.world_mut(), event);
    }

    pub(crate) fn pointer_position(&self) -> Option<InputPosition> {
        projected_pointer_position(self.app.world())
    }

    pub fn take_input_effects(&mut self) -> Vec<SeatInputEffect> {
        take_input_effects(self.app.world_mut())
    }

    pub fn take_host_commands(&mut self) -> Vec<HostCommand> {
        take_host_commands(self.app.world_mut())
    }

    pub fn take_virtual_terminal_switch_request(&mut self) -> Option<i32> {
        take_virtual_terminal_switch_request(self.app.world_mut())
    }

    pub fn take_surface_actions(&mut self) -> Vec<CoreSurfaceAction> {
        take_surface_actions(self.app.world_mut())
            .into_iter()
            .map(core_surface_action)
            .collect()
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
}

fn spawn_compositor_camera(world: &mut World) -> Entity {
    world
        .spawn((
            Camera2d,
            Camera {
                clear_color: ClearColorConfig::Custom(Color::NONE),
                ..Default::default()
            },
            RenderTarget::TextureView(COMPOSITION_VIEW),
            IsDefaultUiCamera,
            // Bevy UI shaders emit linear RGB. The manual sRGB target performs
            // the transfer encoding when those values are written.
            CompositingSpace::Linear,
        ))
        .id()
}

impl CompositionHost for AppShell {
    fn enqueue_surface_event(&mut self, event: PendingSurfaceEvent) -> CompositionDemand {
        AppShell::enqueue_surface_event(self, event)
    }

    fn enqueue_input_event(&mut self, event: RawSeatEvent) {
        AppShell::enqueue_input_event(self, event);
    }

    fn advance_main(&mut self, input_time: u32, composition_advance: bool) -> bool {
        AppShell::advance_main(self, input_time, composition_advance)
    }

    fn render_composition(&mut self, target: wgpu::TextureView, extent: Extent) -> Result<()> {
        AppShell::render_composition(self, target, extent)
    }

    fn set_output_geometry(
        &mut self,
        target: wgpu::TextureView,
        extent: Extent,
        scale_factor: f64,
    ) {
        AppShell::set_output_geometry(self, target, extent, scale_factor);
    }

    fn should_exit(&self) -> bool {
        AppShell::should_exit(self)
    }

    fn pointer_position(&self) -> Option<InputPosition> {
        AppShell::pointer_position(self)
    }

    fn take_input_effects(&mut self) -> Vec<SeatInputEffect> {
        AppShell::take_input_effects(self)
    }

    fn take_host_commands(&mut self) -> Vec<HostCommand> {
        AppShell::take_host_commands(self)
    }

    fn take_virtual_terminal_switch_request(&mut self) -> Option<i32> {
        AppShell::take_virtual_terminal_switch_request(self)
    }

    fn take_surface_actions(&mut self) -> Vec<CoreSurfaceAction> {
        AppShell::take_surface_actions(self)
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

fn app_decoration(decoration: weld_core::surface::WindowDecoration) -> WindowDecoration {
    match decoration {
        weld_core::surface::WindowDecoration::ClientSide => WindowDecoration::ClientSide,
        weld_core::surface::WindowDecoration::ServerSide => WindowDecoration::ServerSide,
    }
}

fn app_interaction(
    interaction: weld_core::surface::WindowInteractionRequestKind,
) -> WindowInteractionRequestKind {
    match interaction {
        weld_core::surface::WindowInteractionRequestKind::Move => {
            WindowInteractionRequestKind::Move
        }
        weld_core::surface::WindowInteractionRequestKind::Resize { edges } => {
            WindowInteractionRequestKind::Resize {
                edges: match edges {
                    weld_core::surface::WindowResizeEdge::Top => WindowResizeEdge::Top,
                    weld_core::surface::WindowResizeEdge::Bottom => WindowResizeEdge::Bottom,
                    weld_core::surface::WindowResizeEdge::Left => WindowResizeEdge::Left,
                    weld_core::surface::WindowResizeEdge::Right => WindowResizeEdge::Right,
                    weld_core::surface::WindowResizeEdge::TopLeft => WindowResizeEdge::TopLeft,
                    weld_core::surface::WindowResizeEdge::BottomLeft => {
                        WindowResizeEdge::BottomLeft
                    }
                    weld_core::surface::WindowResizeEdge::TopRight => WindowResizeEdge::TopRight,
                    weld_core::surface::WindowResizeEdge::BottomRight => {
                        WindowResizeEdge::BottomRight
                    }
                },
            }
        }
        weld_core::surface::WindowInteractionRequestKind::End => WindowInteractionRequestKind::End,
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

fn core_surface_action(action: SurfaceAction) -> CoreSurfaceAction {
    match action {
        SurfaceAction::Close { surface } => CoreSurfaceAction::Close { surface },
        SurfaceAction::Focus { surface } => CoreSurfaceAction::Focus { surface },
        SurfaceAction::Resize {
            surface,
            logical_size,
        } => CoreSurfaceAction::Resize {
            surface,
            logical_size: Extent::new(logical_size.x, logical_size.y),
        },
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

fn insert_manual_view(app: &mut App, texture_view: wgpu::TextureView, width: u32, height: u32) {
    app.world_mut().resource_mut::<ManualTextureViews>().insert(
        COMPOSITION_VIEW,
        ManualTextureView {
            texture_view: texture_view.into(),
            size: UVec2::new(width, height),
            view_format: wgpu::TextureFormat::Rgba8UnormSrgb,
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
        App, Messages, RedrawRequests, SurfaceCompositionDemand, advance_main_app,
        disconnect_render_time, render_composition_app, spawn_compositor_camera,
    };
    use weld_core::{
        CompositionDemand,
        server::{PendingSurfaceEvent, PendingSurfaceEventKind, PendingSurfaceTreeSnapshot},
        surface::SurfaceId,
    };

    #[derive(Resource, Default)]
    struct RenderCount(u32);

    fn request_redraw(mut requests: MessageWriter<RequestRedraw>) {
        requests.write(RequestRedraw);
        requests.write(RequestRedraw);
    }

    fn count_render(mut count: ResMut<RenderCount>) {
        count.0 += 1;
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

    fn snapshot_event(surface: SurfaceId, client_mapped: bool) -> PendingSurfaceEvent {
        PendingSurfaceEvent {
            surface,
            kind: PendingSurfaceEventKind::TreeSnapshot(PendingSurfaceTreeSnapshot {
                client_mapped,
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
        let surface = SurfaceId::new(1);
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
        let surface = SurfaceId::new(1);
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

        let camera = spawn_compositor_camera(app.world_mut());
        let root = app.world_mut().spawn(Node::default()).id();
        app.update();

        assert_eq!(
            app.world()
                .get::<ComputedUiTargetCamera>(root)
                .and_then(ComputedUiTargetCamera::get),
            Some(camera),
        );
    }
}
