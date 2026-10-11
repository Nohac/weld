//! Bounded composition evidence spanning extraction, prepared views and graph execution.

use std::time::{Duration, Instant};

use bevy::{
    app::App,
    camera::{
        Camera, NormalizedRenderTarget, Projection, RenderTarget, primitives::Frustum,
        visibility::VisibleEntities,
    },
    core_pipeline::{Core2dSystems, schedule::Core2d},
    ecs::{query::QueryData, system::SystemParam},
    prelude::*,
    render::{
        Extract, ExtractSchedule, Render, RenderApp, RenderSystems,
        camera::{CameraRenderGraph, ExtractedCamera, SortedCameras},
        render_phase::ViewSortedRenderPhases,
        render_resource::{CachedPipelineState, PipelineCache},
        renderer::{
            CurrentView, PendingCommandBuffers, RenderGraph, RenderGraphSystems, render_system,
        },
        sync_world::RenderEntity,
        texture::ManualTextureViews,
        view::{ExtractedView, ViewTarget},
    },
    ui_render::TransparentUi,
};

use crate::{
    output::{OutputId, RendersOutput, WeldOutput},
    surface::{MappedSurface, SurfaceNode},
};

const TARGET: &str = "weld_render_state";

pub(super) fn install(app: &mut App) {
    if !tracing::enabled!(target: TARGET, tracing::Level::DEBUG) {
        return;
    }
    let Some(render) = app.get_sub_app_mut(RenderApp) else {
        return;
    };
    render
        .init_resource::<Diagnostics>()
        .add_systems(ExtractSchedule, extract_state)
        .add_systems(
            Render,
            (
                reset_graph
                    .in_set(RenderSystems::Render)
                    .before(render_system),
                report_render
                    .in_set(RenderSystems::Render)
                    .after(render_system),
            ),
        )
        .add_systems(Core2d, visit_view.before(Core2dSystems::MainPass))
        .add_systems(
            RenderGraph,
            count_commands
                .after(RenderGraphSystems::Render)
                .before(RenderGraphSystems::Submit),
        );
    tracing::info!(target: TARGET, "composition diagnostics enabled; first eight frames, rate-limited changes, and ten-second heartbeats");
}

/// Changes are coalesced after startup; comparison is against the last emitted state.
struct ReportGate<T> {
    started: Instant,
    last: Option<(Duration, T)>,
}

impl<T> Default for ReportGate<T> {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            last: None,
        }
    }
}

impl<T: PartialEq> ReportGate<T> {
    fn accept(&mut self, frame: u64, now: Duration, value: T) -> bool {
        let emit = self.last.as_ref().is_none_or(|(previous, state)| {
            frame <= 8
                || now.saturating_sub(*previous) >= Duration::from_secs(10)
                || (*state != value && now.saturating_sub(*previous) >= Duration::from_secs(1))
        });
        if emit {
            self.last = Some((now, value));
        }
        emit
    }
}

#[derive(Debug, PartialEq)]
struct MainCamera {
    entity: Entity,
    output: Option<OutputId>,
    render_entity: Option<Entity>,
    active: Option<bool>,
    projection: bool,
    global_transform: bool,
    visible_entities: bool,
    frustum: bool,
    graph: Option<String>,
    target: Option<NormalizedRenderTarget>,
    physical_target: Option<UVec2>,
    physical_viewport: Option<UVec2>,
    manual_view_size: Option<UVec2>,
    manual_view_scale: Option<f32>,
    manual_view_format: Option<wgpu::TextureFormat>,
    clear_color: Option<String>,
}

#[derive(Debug, PartialEq)]
struct MainState {
    cameras: Vec<MainCamera>,
    mapped_surfaces: usize,
    surface_nodes: usize,
    visible_sized_surface_nodes: usize,
}

#[derive(Debug, PartialEq)]
struct RenderCamera {
    entity: Entity,
    target: Option<NormalizedRenderTarget>,
    physical_target: Option<UVec2>,
    extracted_view: bool,
    prepared_target: bool,
    output_attachment: bool,
    attachment_matches_manual_view: Option<bool>,
    output_attachment_used: bool,
    output_format: Option<wgpu::TextureFormat>,
}

#[derive(Debug, Default, PartialEq)]
struct RenderState {
    cameras: Vec<RenderCamera>,
    sorted_cameras: Vec<Entity>,
    core_2d_visits: Vec<Entity>,
    command_buffers_before_submit: usize,
    ui_items: usize,
    pipelines_ready: usize,
    pipelines_queued: usize,
    pipelines_creating: usize,
    pipelines_failed: usize,
    pipeline_errors: Vec<String>,
}

#[derive(Resource, Default)]
struct Diagnostics {
    frame: u64,
    main: ReportGate<MainState>,
    render: ReportGate<RenderState>,
    visits: Vec<Entity>,
    commands: usize,
}

#[derive(QueryData)]
struct InspectCamera {
    entity: Entity,
    output: &'static RendersOutput,
    camera: Option<&'static Camera>,
    target: Option<&'static RenderTarget>,
    graph: Option<&'static CameraRenderGraph>,
    render_entity: Option<&'static RenderEntity>,
    projection: Has<Projection>,
    global_transform: Has<GlobalTransform>,
    visible_entities: Has<VisibleEntities>,
    frustum: Has<Frustum>,
}

#[derive(SystemParam)]
struct MainSources<'w, 's> {
    cameras: Query<'w, 's, InspectCamera>,
    outputs: Query<'w, 's, &'static WeldOutput>,
    mapped: Query<'w, 's, (), With<MappedSurface>>,
    surfaces: Query<
        'w,
        's,
        (
            Option<&'static ComputedNode>,
            Option<&'static InheritedVisibility>,
        ),
        With<SurfaceNode>,
    >,
    views: Res<'w, ManualTextureViews>,
}

fn extract_state(sources: Extract<MainSources>, mut diagnostics: ResMut<Diagnostics>) {
    diagnostics.frame = diagnostics.frame.saturating_add(1);
    let mut cameras = sources
        .cameras
        .iter()
        .map(|item| {
            let target = item.target.and_then(|target| target.normalize(None));
            let view = match &target {
                Some(NormalizedRenderTarget::TextureView(handle)) => sources.views.get(handle),
                _ => None,
            };
            MainCamera {
                entity: item.entity,
                output: sources
                    .outputs
                    .get(item.output.0)
                    .ok()
                    .map(|output| output.id),
                render_entity: item.render_entity.map(RenderEntity::id),
                active: item.camera.map(|camera| camera.is_active),
                projection: item.projection,
                global_transform: item.global_transform,
                visible_entities: item.visible_entities,
                frustum: item.frustum,
                graph: item.graph.map(|graph| format!("{:?}", graph.0)),
                physical_target: item.camera.and_then(Camera::physical_target_size),
                physical_viewport: item.camera.and_then(Camera::physical_viewport_size),
                manual_view_size: view.map(|view| view.size),
                manual_view_scale: view.map(|view| view.scale_factor),
                manual_view_format: view.map(|view| view.view_format),
                clear_color: item
                    .camera
                    .map(|camera| format!("{:?}", camera.clear_color)),
                target,
            }
        })
        .collect::<Vec<_>>();
    cameras.sort_by_key(|camera| camera.entity);
    let state = MainState {
        cameras,
        mapped_surfaces: sources.mapped.iter().count(),
        surface_nodes: sources.surfaces.iter().count(),
        visible_sized_surface_nodes: sources
            .surfaces
            .iter()
            .filter(|(node, visibility)| {
                node.is_some_and(|node| node.size().min_element() > 0.0)
                    && visibility.is_some_and(|visibility| visibility.get())
            })
            .count(),
    };
    let frame = diagnostics.frame;
    let elapsed = diagnostics.main.started.elapsed();
    if diagnostics.main.accept(frame, elapsed, state) {
        tracing::debug!(target: TARGET, frame, state = ?diagnostics.main.last.as_ref().map(|(_, state)| state), "composition main-world state");
    }
}

fn reset_graph(mut diagnostics: ResMut<Diagnostics>) {
    diagnostics.visits.clear();
    diagnostics.commands = 0;
}

fn visit_view(view: Res<CurrentView>, mut diagnostics: ResMut<Diagnostics>) {
    diagnostics.visits.push(view.0);
}

fn count_commands(pending: Res<PendingCommandBuffers>, mut diagnostics: ResMut<Diagnostics>) {
    diagnostics.commands = pending.len();
}

#[derive(QueryData)]
struct InspectRenderCamera {
    entity: Entity,
    camera: &'static ExtractedCamera,
    view: Has<ExtractedView>,
    target: Option<&'static ViewTarget>,
}

fn report_render(
    cameras: Query<InspectRenderCamera>,
    sorted: Res<SortedCameras>,
    views: Res<ManualTextureViews>,
    pipelines: Res<PipelineCache>,
    ui: Res<ViewSortedRenderPhases<TransparentUi>>,
    mut diagnostics: ResMut<Diagnostics>,
) {
    let mut state = RenderState {
        sorted_cameras: sorted.0.iter().map(|camera| camera.entity).collect(),
        core_2d_visits: diagnostics.visits.clone(),
        command_buffers_before_submit: diagnostics.commands,
        ui_items: ui.0.values().map(|phase| phase.items.len()).sum(),
        ..Default::default()
    };
    for item in &cameras {
        let attachment = item.target.and_then(ViewTarget::out_texture);
        let expected = match &item.camera.target {
            Some(NormalizedRenderTarget::TextureView(handle)) => views.get(handle),
            _ => None,
        };
        state.cameras.push(RenderCamera {
            entity: item.entity,
            target: item.camera.target.clone(),
            physical_target: item.camera.physical_target_size,
            extracted_view: item.view,
            prepared_target: item.target.is_some(),
            output_attachment: attachment.is_some(),
            attachment_matches_manual_view: expected.map(|view| {
                attachment.is_some_and(|attachment| attachment.id() == view.texture_view.id())
            }),
            output_attachment_used: item.target.is_some_and(ViewTarget::needs_present),
            output_format: item.target.and_then(ViewTarget::out_texture_view_format),
        });
    }
    state.cameras.sort_by_key(|camera| camera.entity);
    for pipeline in pipelines.pipelines() {
        match &pipeline.state {
            CachedPipelineState::Ok(_) => state.pipelines_ready += 1,
            CachedPipelineState::Queued => state.pipelines_queued += 1,
            CachedPipelineState::Creating(_) => state.pipelines_creating += 1,
            CachedPipelineState::Err(error) => {
                state.pipelines_failed += 1;
                if state.pipeline_errors.len() < 4 {
                    state.pipeline_errors.push(format!("{error:?}"));
                }
            }
        }
    }
    let frame = diagnostics.frame;
    let elapsed = diagnostics.render.started.elapsed();
    if diagnostics.render.accept(frame, elapsed, state) {
        tracing::debug!(target: TARGET, frame, state = ?diagnostics.render.last.as_ref().map(|(_, state)| state), "composition render-world state");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_startup_coalesces_changes_and_keeps_heartbeats() {
        let mut gate = ReportGate::default();
        for frame in 1..=8 {
            assert!(gate.accept(frame, Duration::ZERO, 0));
        }
        assert!(!gate.accept(9, Duration::from_millis(100), 1));
        assert!(!gate.accept(10, Duration::from_millis(500), 2));
        assert!(gate.accept(11, Duration::from_secs(1), 2));
        assert!(!gate.accept(12, Duration::from_secs(2), 2));
        assert!(gate.accept(13, Duration::from_secs(11), 2));
    }
}
