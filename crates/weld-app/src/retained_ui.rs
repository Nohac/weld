//! Reuses prepared UI geometry while scene dependencies remain unchanged.

use anyhow::{Context, Result};
use bevy::{
    app::{App, Inherited, Propagate},
    asset::{Asset, AssetEvent},
    camera::{Camera, Camera3d, ClearColor, visibility::VisibleEntities},
    ecs::{
        change_detection::{DetectChanges, Tick},
        component::{Component, Components},
        entity::Entity,
        message::{MessageCursor, Messages},
        query::{Or, With, Without},
        resource::{IsResource, Resource},
        schedule::{IntoScheduleConfigs, Schedule, ScheduleLabel, SingleThreadedExecutor},
        system::{Commands, Local, Query, Res, ResMut, SystemChangeTick},
        world::{EntityRef, World},
    },
    image::{Image, TextureAtlasLayout},
    mesh::{Mesh2d, Mesh3d},
    render::{
        ExtractSchedule, Render, RenderApp, RenderStartup, RenderSystems,
        render_asset::RenderAssets, render_resource::PipelineCache,
        sync_world::TemporaryRenderEntity, texture::GpuImage,
    },
    shader::Shader,
    sprite::Sprite,
    text::{Font, TextSpan},
    ui::{ComputedUiRenderTargetInfo, ComputedUiTargetCamera, Node, UiScale, widget::ViewportNode},
    ui_render::RenderUiSystems,
};

/// Requests full preparation for custom rendering driven by external state or time.
///
/// Attach to an entity while its renderer requires preparation on every frame.
#[derive(Component, Default)]
pub struct ContinuousUiPreparation;

/// Controls prepared-UI reuse. Disable when installing a custom render pipeline.
#[derive(Resource)]
pub struct RetainedUiSettings {
    /// Reuse prepared drawing data when tracked UI dependencies are unchanged.
    pub enabled: bool,
}

/// Preparation counters for diagnostics and render-path validation.
#[derive(Resource, Default, Clone, Copy, Debug)]
pub struct RetainedUiStats {
    /// Frames submitted through full UI extraction and preparation.
    pub prepared_frames: u64,
    /// Frames submitted with retained UI batches.
    pub reused_frames: u64,
    /// Whether the latest frame selected full preparation.
    pub last_prepared: bool,
}

impl Default for RetainedUiSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Resource, Default)]
struct SceneChanges {
    dirty: bool,
}

#[derive(Resource)]
struct PreparedUi {
    rebuild: bool,
    retain: bool,
    settling: u8,
    images_tick: Option<Tick>,
}

#[derive(Component)]
struct RetainedRenderEntity;

#[derive(ScheduleLabel, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CheckUiChanges;

pub(crate) fn install(app: &mut App) {
    let mut check = Schedule::new(CheckUiChanges);
    check.set_executor(SingleThreadedExecutor::new());
    check.add_systems((
        detect_components,
        detect_assets::<Image>,
        detect_assets::<Shader>,
        detect_assets::<Font>,
        detect_assets::<TextureAtlasLayout>,
        detect_resource::<UiScale>,
        detect_resource::<ClearColor>,
    ));
    app.init_resource::<RetainedUiSettings>()
        .init_resource::<SceneChanges>()
        .init_resource::<RetainedUiStats>()
        .add_schedule(check);
    let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
        return;
    };
    render_app.insert_resource(PreparedUi {
        rebuild: true,
        retain: false,
        settling: 2,
        images_tick: None,
    });
    for set in [
        RenderUiSystems::ExtractCameraViews,
        RenderUiSystems::ExtractBoxShadows,
        RenderUiSystems::ExtractBackgrounds,
        RenderUiSystems::ExtractImages,
        RenderUiSystems::ExtractTextureSlice,
        RenderUiSystems::ExtractBorders,
        RenderUiSystems::ExtractViewportNodes,
        RenderUiSystems::ExtractTextBackgrounds,
        RenderUiSystems::ExtractTextShadows,
        RenderUiSystems::ExtractText,
        RenderUiSystems::ExtractCursor,
        RenderUiSystems::ExtractDebug,
        RenderUiSystems::ExtractGradient,
    ] {
        render_app.configure_sets(ExtractSchedule, set.run_if(needs_preparation));
    }
    for set in [
        RenderSystems::Queue,
        RenderSystems::PhaseSort,
        RenderSystems::PrepareBindGroups,
    ] {
        render_app.configure_sets(Render, set.run_if(needs_preparation));
    }
    render_app
        .add_systems(RenderStartup, reset_renderer)
        .add_systems(
            Render,
            retain_entities
                .after(RenderSystems::Render)
                .before(RenderSystems::PostCleanup)
                .run_if(retains_preparation),
        );
}

pub(crate) fn watch_asset<A: Asset>(app: &mut App) {
    app.add_systems(CheckUiChanges, detect_assets::<A>);
}

fn detect_assets<A: Asset>(
    messages: Option<Res<Messages<AssetEvent<A>>>>,
    mut cursor: Local<MessageCursor<AssetEvent<A>>>,
    mut changes: ResMut<SceneChanges>,
) {
    if let Some(messages) = messages {
        if cursor.len(&messages) > 0 || cursor.missed_messages(&messages) > 0 {
            changes.dirty = true;
            tracing::trace!(target: "weld_render_cache", asset = std::any::type_name::<A>(), "UI asset changed");
        }
        cursor.clear(&messages);
    }
}

fn detect_resource<R: Resource>(
    resource: Option<Res<R>>,
    mut present: Local<bool>,
    mut changes: ResMut<SceneChanges>,
) {
    if resource.as_ref().is_some_and(|value| value.is_changed()) || resource.is_some() != *present {
        changes.dirty = true;
        tracing::trace!(target: "weld_render_cache", resource = std::any::type_name::<R>(), "UI dependency changed");
    }
    *present = resource.is_some();
}

type VisualEntities<'w, 's> = Query<
    'w,
    's,
    EntityRef<'static>,
    (
        Or<(With<Node>, With<Camera>, With<TextSpan>)>,
        Without<IsResource>,
    ),
>;

type ContinuousRender = Or<(
    With<ContinuousUiPreparation>,
    With<Camera3d>,
    With<ViewportNode>,
    With<Mesh2d>,
    With<Mesh3d>,
    With<Sprite>,
)>;

fn detect_components(
    entities: VisualEntities,
    components: &Components,
    ticks: SystemChangeTick,
    mut counts: Local<(usize, usize)>,
    mut changes: ResMut<SceneChanges>,
    continuous: Query<(), ContinuousRender>,
) {
    // Bevy rewrites propagation intermediates every update. Their final computed
    // components carry meaningful change ticks. UI extraction uses its own stack;
    // VisibleEntities is the mesh/sprite visibility list, rebuilt each update.
    let intermediates = [
        components.component_id::<VisibleEntities>(),
        components.component_id::<Propagate<ComputedUiTargetCamera>>(),
        components.component_id::<Inherited<ComputedUiTargetCamera>>(),
        components.component_id::<Propagate<ComputedUiRenderTargetInfo>>(),
        components.component_id::<Inherited<ComputedUiRenderTargetInfo>>(),
    ];
    let mut current = (0, 0);
    for entity in &entities {
        current.0 += 1;
        for &component in entity.archetype().components() {
            current.1 += 1;
            if intermediates.contains(&Some(component)) {
                continue;
            }
            if entity
                .get_change_ticks_by_id(component)
                .is_some_and(|change| change.is_changed(ticks.last_run(), ticks.this_run()))
            {
                changes.dirty = true;
                tracing::trace!(target: "weld_render_cache", component = ?components.get_info(component).map(|info| info.name()), "UI dependency changed");
            }
        }
    }
    if current != *counts || !continuous.is_empty() {
        changes.dirty = true;
        tracing::trace!(target: "weld_render_cache", structural = (current != *counts), continuous = !continuous.is_empty(), "UI preparation requirement");
    }
    *counts = current;
}

fn needs_preparation(state: Res<PreparedUi>) -> bool {
    state.rebuild
}

fn retains_preparation(state: Res<PreparedUi>) -> bool {
    state.rebuild && state.retain
}

fn retain_entities(mut commands: Commands, entities: Query<Entity, With<TemporaryRenderEntity>>) {
    for entity in &entities {
        commands
            .entity(entity)
            .remove::<TemporaryRenderEntity>()
            .insert(RetainedRenderEntity);
    }
}

fn retire_entities(world: &mut World) {
    let mut query = world.query_filtered::<Entity, With<RetainedRenderEntity>>();
    let mut entities: Vec<_> = query.iter(world).collect();
    entities.sort_unstable_by_key(|entity| entity.index());
    for entity in entities.into_iter().rev() {
        world.despawn(entity);
    }
}

fn reset_renderer(world: &mut World) {
    retire_entities(world);
    let retain = world.resource::<PreparedUi>().retain;
    world.insert_resource(PreparedUi {
        rebuild: true,
        retain,
        settling: 2,
        images_tick: None,
    });
}

pub(crate) fn render(app: &mut App) -> Result<()> {
    app.world_mut().run_schedule(CheckUiChanges);
    let dirty = std::mem::take(&mut app.world_mut().resource_mut::<SceneChanges>().dirty);
    let enabled = app.world().resource::<RetainedUiSettings>().enabled;
    let render_app = app
        .get_sub_app_mut(RenderApp)
        .context("retained UI requires RenderApp")?;
    let images_tick = render_app
        .world()
        .get_resource_ref::<RenderAssets<GpuImage>>()
        .map(|images| images.last_changed());
    let pending_pipeline = render_app
        .world()
        .get_resource::<PipelineCache>()
        .is_some_and(|cache| cache.waiting_pipelines().next().is_some());
    let mut state = render_app.world_mut().resource_mut::<PreparedUi>();
    let images_changed = state.images_tick != images_tick;
    tracing::trace!(target: "weld_render_cache", scene_dirty = dirty, pending_pipeline, images_changed, "UI render dependencies");
    let dirty = dirty || pending_pipeline || images_changed || state.retain != enabled;
    state.images_tick = images_tick;
    if dirty {
        state.settling = 2;
    }
    state.rebuild = !enabled || dirty || state.settling > 0;
    state.retain = enabled;
    state.settling = state.settling.saturating_sub(1);
    let rebuild = state.rebuild;
    if rebuild {
        retire_entities(render_app.world_mut());
    }
    tracing::trace!(target: "weld_render_cache", rebuild, dirty, "UI preparation decision");
    app.update_sub_app_by_label(RenderApp);
    let rebuild = app
        .sub_app(RenderApp)
        .world()
        .resource::<PreparedUi>()
        .rebuild;
    let mut stats = app.world_mut().resource_mut::<RetainedUiStats>();
    stats.last_prepared = rebuild;
    if rebuild {
        stats.prepared_frames = stats.prepared_frames.saturating_add(1);
    } else {
        stats.reused_frames = stats.reused_frames.saturating_add(1);
    }
    app.world_mut().clear_trackers();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Component)]
    struct Style(u8);

    #[test]
    fn expired_asset_events_still_invalidate_the_cached_scene() {
        let mut app = App::new();
        app.init_resource::<SceneChanges>()
            .init_resource::<Messages<AssetEvent<Image>>>()
            .add_systems(CheckUiChanges, detect_assets::<Image>);
        assert!(!check(&mut app));
        {
            let mut messages = app
                .world_mut()
                .resource_mut::<Messages<AssetEvent<Image>>>();
            messages.write(AssetEvent::Modified {
                id: Default::default(),
            });
            for _ in 0..5 {
                messages.update();
            }
        }
        assert!(check(&mut app));
        assert!(!check(&mut app));
    }

    fn check(app: &mut App) -> bool {
        app.world_mut().run_schedule(CheckUiChanges);
        let dirty = std::mem::take(&mut app.world_mut().resource_mut::<SceneChanges>().dirty);
        app.world_mut().clear_trackers();
        dirty
    }

    #[test]
    fn detects_style_and_removals_across_multiple_main_updates() {
        let mut app = App::new();
        app.init_resource::<SceneChanges>()
            .add_systems(CheckUiChanges, detect_components);
        let entity = app.world_mut().spawn((Node::default(), Style(0))).id();
        assert!(check(&mut app));
        assert!(!check(&mut app));
        app.world_mut().get_mut::<Style>(entity).expect("style").0 = 1;
        assert!(check(&mut app));
        assert!(!check(&mut app));
        app.world_mut().entity_mut(entity).remove::<Style>();
        for _ in 0..5 {
            app.world_mut().clear_trackers();
        }
        assert!(check(&mut app));
        assert!(!check(&mut app));
        app.world_mut().entity_mut(entity).despawn();
        app.world_mut().spawn(Node::default());
        assert!(
            check(&mut app),
            "replacement is detected even when entity counts match"
        );
        assert!(!check(&mut app));
    }

    #[test]
    fn retired_batches_leave_persistent_entities_alive() {
        let mut world = World::new();
        let transient = world.spawn(TemporaryRenderEntity).id();
        let persistent = world.spawn_empty().id();
        let mut schedule = Schedule::default();
        schedule.add_systems(retain_entities);
        schedule.run(&mut world);
        assert!(world.get::<TemporaryRenderEntity>(transient).is_none());
        assert!(world.get::<RetainedRenderEntity>(transient).is_some());
        retire_entities(&mut world);
        assert!(world.get_entity(transient).is_err());
        assert!(world.get_entity(persistent).is_ok());
    }
}
