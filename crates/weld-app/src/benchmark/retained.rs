//! Fixed-scene experiment retaining Bevy's prepared UI between full render updates.
//!
//! The fixture refreshes every 60 frames after warmup. Production use requires
//! explicit scene/asset invalidation and device-recovery handling.

use anyhow::{Context, Result, ensure};
use bevy::{
    app::App,
    ecs::{
        component::Component,
        entity::Entity,
        query::With,
        resource::Resource,
        schedule::{IntoScheduleConfigs, Schedule, ScheduleLabel},
        system::{Commands, Query, ResMut},
        world::World,
    },
    render::{
        Render, RenderApp, RenderSystems,
        render_resource::PipelineCache,
        renderer::render_system,
        sync_world::TemporaryRenderEntity,
        texture::{ManualTextureViews, update_texture_cache_system},
        view::{clear_view_attachments, prepare_view_attachments, prepare_view_targets},
    },
};

use weld_core::{
    host::{CompositionFrame, CompositionOutputFrame, CompositionTargetView},
    renderer::read_owned_frame_rgba,
};

#[derive(Resource)]
struct RetainedProbe {
    frames: u64,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

#[derive(Component)]
struct RetainedRenderEntity;

#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
struct ReplayUi;

pub(crate) fn install(app: &mut App, device: &wgpu::Device, queue: &wgpu::Queue) -> Result<()> {
    let render_app = app
        .get_sub_app_mut(RenderApp)
        .context("retained UI probe requires RenderApp")?;
    // Keep the batches and UI camera created by extraction alive through cleanup.
    render_app.add_systems(
        Render,
        retain_entities
            .after(RenderSystems::Render)
            .before(RenderSystems::PostCleanup),
    );
    let mut replay = Schedule::new(ReplayUi);
    replay.add_systems(
        (
            clear_view_attachments,
            prepare_view_attachments,
            prepare_view_targets,
            process_pipelines,
            render_system,
            update_texture_cache_system,
        )
            .chain(),
    );
    crate::surface_impl::binding::benchmark::configure(&mut replay);
    render_app.add_schedule(replay);
    app.insert_resource(RetainedProbe {
        frames: 0,
        device: device.clone(),
        queue: queue.clone(),
    });
    Ok(())
}

fn process_pipelines(mut cache: ResMut<PipelineCache>) {
    cache.process_queue();
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

fn rebuild(frame: u64) -> bool {
    frame < 120 || frame.is_multiple_of(60)
}

pub(super) fn render(app: &mut App, frames: &[CompositionOutputFrame]) -> Result<bool> {
    let Some(mut probe) = app.world_mut().remove_resource::<RetainedProbe>() else {
        return Ok(false);
    };
    let result = render_frame(app, frames, &mut probe);
    app.world_mut().insert_resource(probe);
    result.map(|()| true)
}

fn render_frame(
    app: &mut App,
    frames: &[CompositionOutputFrame],
    probe: &mut RetainedProbe,
) -> Result<()> {
    let refresh = rebuild(probe.frames);
    let validate = (121..=123).contains(&probe.frames);
    probe.frames = probe.frames.saturating_add(1);
    if refresh {
        let render_app = app
            .get_sub_app_mut(RenderApp)
            .context("retained UI probe lost RenderApp")?;
        retire_entities(render_app.world_mut());
        app.update_sub_app_by_label(RenderApp);
    } else {
        // Every output acquisition can provide a different backing texture.
        let views = app.world().resource::<ManualTextureViews>().clone();
        let render_app = app
            .get_sub_app_mut(RenderApp)
            .context("retained UI probe lost RenderApp")?;
        render_app.world_mut().insert_resource(views);
        render_app.world_mut().run_schedule(ReplayUi);
        render_app.world_mut().clear_trackers();
        if validate {
            validate_replay(app, frames, probe)?;
        }
    }
    app.world_mut().clear_trackers();
    Ok(())
}

fn validate_replay(
    app: &mut App,
    frames: &[CompositionOutputFrame],
    probe: &RetainedProbe,
) -> Result<()> {
    ensure!(frames.len() == 1, "retained validation requires one output");
    let frame = &frames[0].frame;
    let replayed = read_owned_frame_rgba(&probe.device, &probe.queue, frame)?;

    // Rotate the backing texture without changing the camera's target handle.
    let extent = frame.target().extent();
    let texture = probe.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("retained replay rotation validation"),
        size: wgpu::Extent3d {
            width: extent.width,
            height: extent.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: frame.target().format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    let rotated = CompositionFrame::owned(
        CompositionTargetView::new(view.clone(), extent, frame.target().format()),
        texture,
    );
    let mut views = app.world().resource::<ManualTextureViews>().clone();
    ensure!(
        views.len() == 1,
        "retained validation requires one manual view"
    );
    for target in views.values_mut() {
        target.texture_view = view.clone().into();
    }
    let render_app = app
        .get_sub_app_mut(RenderApp)
        .context("missing RenderApp")?;
    render_app.world_mut().insert_resource(views);
    render_app.world_mut().run_schedule(ReplayUi);
    let rotated_pixels = read_owned_frame_rgba(&probe.device, &probe.queue, &rotated)?;
    ensure!(
        replayed == rotated_pixels,
        "retained replay changed pixels on output buffer rotation"
    );

    retire_entities(render_app.world_mut());
    app.update_sub_app_by_label(RenderApp);
    let reference = read_owned_frame_rgba(&probe.device, &probe.queue, frame)?;
    let different = replayed
        .iter()
        .zip(&reference)
        .filter(|(a, b)| a != b)
        .count();
    ensure!(
        different == 0,
        "retained replay differs from full preparation in {different} channels"
    );
    println!(
        "RETAINED_VALIDATION frame={} pixels={} replay_matches_full=true rotated_target_matches=true",
        probe.frames,
        replayed.len() / 4
    );
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn refresh_and_validate(app: &mut App) {
        app.world_mut().resource_mut::<RetainedProbe>().frames = 120;
    }

    #[test]
    fn retained_entities_survive_replay_and_are_retired_before_rebuild() {
        let mut world = World::new();
        let retained = world.spawn(TemporaryRenderEntity).id();
        let persistent = world.spawn_empty().id();
        let mut schedule = Schedule::default();
        schedule.add_systems(retain_entities);
        schedule.run(&mut world);
        assert!(world.get::<TemporaryRenderEntity>(retained).is_none());
        assert!(world.get::<RetainedRenderEntity>(retained).is_some());
        retire_entities(&mut world);
        assert!(world.get_entity(retained).is_err());
        assert!(world.get_entity(persistent).is_ok());
    }
}
