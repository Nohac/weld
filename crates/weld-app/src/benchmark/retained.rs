//! GPU equivalence checks for the production retained UI path during fixture warmup.

use anyhow::{Result, ensure};
use bevy::{app::App, ecs::resource::Resource, render::texture::ManualTextureViews};
use weld_core::{
    host::{CompositionFrame, CompositionOutputFrame, CompositionTargetView},
    renderer::read_owned_frame_rgba,
};

use crate::retained_ui::{RetainedUiSettings, RetainedUiStats};

#[derive(Resource)]
struct RetainedProbe {
    frames: u64,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

pub(crate) fn install(app: &mut App, device: &wgpu::Device, queue: &wgpu::Queue) -> Result<()> {
    app.insert_resource(RetainedProbe {
        frames: 0,
        device: device.clone(),
        queue: queue.clone(),
    });
    Ok(())
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
    crate::retained_ui::render(app)?;
    let validate = (121..=123).contains(&probe.frames);
    probe.frames = probe.frames.saturating_add(1);
    if validate {
        validate_replay(app, frames, probe)?;
    }
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
    let original_views = app.world().resource::<ManualTextureViews>().clone();
    let mut views = original_views.clone();
    ensure!(
        views.len() == 1,
        "retained validation requires one manual view"
    );
    for target in views.values_mut() {
        target.texture_view = view.clone().into();
    }
    app.world_mut().insert_resource(views);
    let result = crate::retained_ui::render(app);
    app.world_mut().insert_resource(original_views);
    result?;
    let rotated_pixels = read_owned_frame_rgba(&probe.device, &probe.queue, &rotated)?;
    ensure!(
        replayed == rotated_pixels,
        "retained replay changed pixels on output buffer rotation"
    );

    app.world_mut().resource_mut::<RetainedUiSettings>().enabled = false;
    let result = crate::retained_ui::render(app);
    app.world_mut().resource_mut::<RetainedUiSettings>().enabled = true;
    result?;
    let reference = read_owned_frame_rgba(&probe.device, &probe.queue, frame)?;
    let different = replayed
        .iter()
        .zip(&reference)
        .filter(|(a, b)| a != b)
        .count();
    ensure!(
        replayed == reference,
        "retained replay differs from full preparation in {different} channels"
    );
    let stats = app.world().resource::<RetainedUiStats>();
    if probe.frames == 124 {
        ensure!(stats.reused_frames > 0, "fixture never reused prepared UI");
    }
    println!(
        "RETAINED_VALIDATION frame={} pixels={} replay_matches_full=true rotated_target_matches=true prepared={} reused={}",
        probe.frames,
        replayed.len() / 4,
        stats.prepared_frames,
        stats.reused_frames
    );
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn refresh_and_validate(app: &mut App) {
        app.world_mut().resource_mut::<RetainedProbe>().frames = 120;
    }
}
