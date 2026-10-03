//! Diagnostic substitutions after the production bridge has promoted buffers.

use std::collections::HashMap;

use anyhow::{Context, Result, ensure};
use bevy::{
    app::{App, AppLabel},
    asset::AssetId,
    ecs::{
        entity::Entity,
        query::{QueryState, With},
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::Res,
    },
    image::Image,
    render::{
        ExtractSchedule, RenderApp,
        render_asset::RenderAssets,
        renderer::{RenderGraph, RenderGraphSystems},
        sync_world::TemporaryRenderEntity,
        texture::GpuImage,
    },
};
use weld_core::{
    benchmark::SurfaceBlitter,
    host::{CompositionOutputFrame, RenderContext},
};

/// Which render-world work precedes the diagnostic direct surface draw.
#[derive(Clone, Copy, Debug)]
pub enum PresentationProbe {
    /// Keep production main-world work, image registration and buffer lifetime.
    Direct,
    /// Also extract and apply render-world commands, retiring temporary entities.
    ExtractThenDirect,
    /// Run extraction, preparation and cleanup with the camera graph gated off.
    PrepareThenDirect,
    /// Also run the normal renderer with no mounted client UI, then draw clients.
    RenderThenDirect,
}

#[derive(Resource)]
struct DirectPresentation {
    probe: PresentationProbe,
    warmup_frames: u8,
    blitter: SurfaceBlitter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    bindings: HashMap<AssetId<Image>, wgpu::BindGroup>,
    temporary_query: Option<QueryState<Entity, With<TemporaryRenderEntity>>>,
    temporary_entities: Vec<Entity>,
}

#[derive(Resource)]
struct CameraDrawEnabled(bool);

fn camera_draw_enabled(enabled: Res<CameraDrawEnabled>) -> bool {
    enabled.0
}

/// Select a fixed-row diagnostic draw on the production image-import path.
pub(super) fn install_presentation_probe(
    app: &mut App,
    context: &RenderContext,
    probe: PresentationProbe,
) -> Result<()> {
    if matches!(probe, PresentationProbe::PrepareThenDirect) {
        app.get_sub_app_mut(RenderApp)
            .context("diagnostic probe requires RenderApp")?
            .insert_resource(CameraDrawEnabled(true))
            .configure_sets(
                RenderGraph,
                RenderGraphSystems::Render.run_if(camera_draw_enabled),
            );
    }
    app.insert_resource(DirectPresentation {
        probe,
        warmup_frames: 0,
        blitter: SurfaceBlitter::for_benchmark_client(&context.device, context.composition_format),
        device: context.device.clone(),
        queue: context.queue.clone(),
        bindings: HashMap::new(),
        temporary_query: None,
        temporary_entities: Vec::new(),
    });
    Ok(())
}

pub(crate) fn render_probe(app: &mut App, frames: &[CompositionOutputFrame]) -> Result<bool> {
    let Some(mut direct) = app.world_mut().remove_resource::<DirectPresentation>() else {
        return Ok(false);
    };
    let result = render(app, frames, &mut direct);
    app.world_mut().insert_resource(direct);
    result.map(|()| true)
}

fn render(
    app: &mut App,
    frames: &[CompositionOutputFrame],
    direct: &mut DirectPresentation,
) -> Result<()> {
    ensure!(
        frames.len() == 1,
        "diagnostic direct presentation requires one output"
    );
    // Let startup, pipelines and view resources settle through five normal
    // render frames. The fixture warmup excludes these from steady measurements.
    if direct.warmup_frames < 5
        || matches!(
            direct.probe,
            PresentationProbe::RenderThenDirect | PresentationProbe::PrepareThenDirect
        )
    {
        if matches!(direct.probe, PresentationProbe::PrepareThenDirect) {
            app.sub_app_mut(RenderApp)
                .world_mut()
                .resource_mut::<CameraDrawEnabled>()
                .0 = direct.warmup_frames < 5;
        }
        app.update_sub_app_by_label(RenderApp);
        direct.warmup_frames = direct.warmup_frames.saturating_add(1).min(5);
    } else if matches!(direct.probe, PresentationProbe::ExtractThenDirect) {
        let apps = app.sub_apps_mut();
        let render = apps
            .sub_apps
            .get_mut(&RenderApp.intern())
            .context("missing RenderApp")?;
        render.extract(apps.main.world_mut());
        let world = render.world_mut();
        world.try_schedule_scope(ExtractSchedule, |world, schedule| {
            schedule.apply_deferred(world)
        })?;
        let query = direct
            .temporary_query
            .get_or_insert_with(|| world.query_filtered());
        direct.temporary_entities.extend(query.iter(world));
        direct
            .temporary_entities
            .sort_unstable_by_key(|entity| entity.index());
        for entity in direct.temporary_entities.drain(..).rev() {
            world.despawn(entity);
        }
        world.clear_trackers();
    }
    let images = crate::surface_impl::benchmark_root_images(app.world());
    let gpu_images = app
        .get_sub_app(RenderApp)
        .context("diagnostic presenter requires RenderApp")?
        .world()
        .resource::<RenderAssets<GpuImage>>();
    direct
        .bindings
        .retain(|id, _| gpu_images.get(*id).is_some());
    let target = frames[0].frame.target();
    let extent = target.extent();
    let mut encoder = direct.device.create_command_encoder(&Default::default());
    {
        let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("bridge diagnostic clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target.view(),
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
    let width = extent.width as f32 / images.len().max(1) as f32;
    for (index, id) in images.into_iter().enumerate() {
        let image = gpu_images
            .get(id)
            .context("promoted root has no GPU image")?;
        let binding = direct.bindings.entry(id).or_insert_with(|| {
            direct.blitter.create_bind_group(
                &direct.device,
                "bridge diagnostic client",
                &image.texture_view,
            )
        });
        direct.blitter.encode_overlay(
            &mut encoder,
            "bridge diagnostic client",
            target.view(),
            binding,
            (index as f32 * width, 0.0, width, extent.height as f32),
            [(0, 0, extent.width, extent.height)],
        );
    }
    direct.queue.submit([encoder.finish()]);
    app.world_mut().clear_trackers();
    Ok(())
}
