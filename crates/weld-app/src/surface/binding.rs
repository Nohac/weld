//! Render-world selection and caching for stable surface-layer materials.

use std::collections::{HashMap, HashSet};

use bevy::{
    app::{App, SubApp},
    asset::{AssetId, Assets},
    ecs::{
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Res, ResMut, SystemParam},
    },
    image::Image,
    math::UVec2,
    render::{
        Render, RenderSystems,
        render_asset::RenderAssets,
        render_phase::{PhaseItem, ViewSortedRenderPhases},
        render_resource::{
            BindGroup, BindGroupEntry, BindingResource, PipelineCache, UniformBuffer,
        },
        renderer::{RenderDevice, RenderQueue},
        texture::GpuImage,
    },
    ui_render::{PreparedUiMaterial, TransparentUi, UiMaterialBatch, UiMaterialPipeline},
};
use tracing::warn;

use super::{
    MaterialSelectorRegistry, SurfaceImageEncoding, SurfaceMaterialParameters, SurfaceParameterKey,
    SurfaceRegistry, SurfaceUiMaterial, encoding_flag,
};

#[derive(Clone, Copy)]
struct PublishedSurfaceBinding {
    image: AssetId<Image>,
    generation: u64,
    parameters: SurfaceMaterialParameters,
}

#[derive(Resource, Clone, Default)]
struct PublishedSurfaceBindings {
    desired: HashMap<AssetId<SurfaceUiMaterial>, PublishedSurfaceBinding>,
    valid_images: HashSet<AssetId<Image>>,
    material_parameters: HashMap<AssetId<SurfaceUiMaterial>, SurfaceParameterKey>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SurfaceBindingCacheKey {
    material: AssetId<SurfaceUiMaterial>,
    image: AssetId<Image>,
    generation: u64,
    parameters: SurfaceParameterKey,
}

#[derive(Resource, Default)]
struct SurfaceBindingCache {
    bindings: HashMap<SurfaceBindingCacheKey, BindGroup>,
    parameters: HashMap<AssetId<SurfaceUiMaterial>, UniformBuffer<SurfaceMaterialParameters>>,
}

/// Materials whose clipped draw batches and current image bindings were ready.
pub(super) fn sampled_materials(app: &App) -> HashSet<AssetId<SurfaceUiMaterial>> {
    let mut sampled = HashSet::new();
    let Some(render) = app.get_sub_app(bevy::render::RenderApp) else {
        return sampled;
    };
    let world = render.world();
    let (Some(phases), Some(pipelines), Some(published), Some(bindings)) = (
        world.get_resource::<ViewSortedRenderPhases<TransparentUi>>(),
        world.get_resource::<PipelineCache>(),
        world.get_resource::<PublishedSurfaceBindings>(),
        world.get_resource::<SurfaceBindingCache>(),
    ) else {
        return sampled;
    };
    for phase in phases.values() {
        for item in phase.items.values() {
            if pipelines.get_render_pipeline(item.pipeline).is_none() {
                continue;
            }
            let Some(batch) = world.get::<UiMaterialBatch<SurfaceUiMaterial>>(item.entity()) else {
                continue;
            };
            if batch.range.is_empty() {
                continue;
            }
            let Some(desired) = published.desired.get(&batch.material) else {
                continue;
            };
            if bindings.bindings.contains_key(&SurfaceBindingCacheKey {
                material: batch.material,
                image: desired.image,
                generation: desired.generation,
                parameters: desired.parameters.into(),
            }) {
                sampled.insert(batch.material);
            }
        }
    }
    sampled
}

pub(super) fn configure_render_app(render_app: &mut SubApp) {
    render_app
        .init_resource::<PublishedSurfaceBindings>()
        .init_resource::<SurfaceBindingCache>()
        .add_systems(
            Render,
            prepare_surface_bindings
                .after(RenderSystems::Prepare)
                .before(RenderSystems::Render),
        );
}

pub(crate) fn publish_surface_bindings(
    app: &mut App,
    mut valid_images: HashSet<AssetId<Image>>,
) -> bool {
    let mut assets_changed = false;
    let selectors = app
        .world()
        .get_resource::<MaterialSelectorRegistry>()
        .map(|registry| registry.0.clone())
        .unwrap_or_default();
    let mut resolved = Vec::with_capacity(selectors.len());
    for (material_id, selector) in selectors {
        let Some(buffer) = app
            .world()
            .get_resource::<SurfaceRegistry>()
            .and_then(|registry| registry.entries.get(&selector.surface))
            .and_then(|entry| entry.buffers.get(&selector.layer))
        else {
            continue;
        };
        let Some(material) = app
            .world()
            .get_resource::<Assets<SurfaceUiMaterial>>()
            .and_then(|materials| materials.get(material_id))
            .cloned()
        else {
            continue;
        };
        let (image, generation, encoding, y_inverted) = if let Some(displayed) =
            &buffer.displayed_dmabuf
            && valid_images.contains(&displayed.image.id())
        {
            (
                displayed.image.id(),
                0,
                displayed.encoding,
                displayed.y_inverted,
            )
        } else if buffer.encoding != SurfaceImageEncoding::Unbound {
            (buffer.image.id(), buffer.generation, buffer.encoding, false)
        } else {
            (buffer.image.id(), 0, SurfaceImageEncoding::Unbound, false)
        };
        valid_images.insert(buffer.image.id());
        resolved.push((
            material_id,
            selector,
            material,
            image,
            generation,
            encoding,
            y_inverted,
        ));
    }

    let mut published = PublishedSurfaceBindings {
        valid_images,
        ..Default::default()
    };
    for (material_id, selector, mut material, image, generation, encoding, y_inverted) in resolved {
        material.parameters.flags = UVec2::new(encoding_flag(encoding), u32::from(y_inverted));
        let parameters = material.parameters;
        if let Some(mut materials) = app
            .world_mut()
            .get_resource_mut::<Assets<SurfaceUiMaterial>>()
            && materials.get(material_id) != Some(&material)
        {
            match materials.insert(material_id, material) {
                Ok(()) => assets_changed = true,
                Err(error) => warn!(%error, ?material_id, "could not refresh a surface material"),
            }
        }
        if let Some(mut selectors) = app
            .world_mut()
            .get_resource_mut::<MaterialSelectorRegistry>()
            && let Some(current) = selectors.0.get_mut(&material_id)
            && current.surface == selector.surface
            && current.layer == selector.layer
        {
            current.parameters = parameters.into();
        }
        published.desired.insert(
            material_id,
            PublishedSurfaceBinding {
                image,
                generation,
                parameters,
            },
        );
        published
            .material_parameters
            .insert(material_id, parameters.into());
    }
    if let Some(render_app) = app.get_sub_app_mut(bevy::render::RenderApp) {
        render_app.world_mut().insert_resource(published);
    }
    assets_changed
}

#[derive(SystemParam)]
struct SurfaceBindingResources<'w> {
    device: Res<'w, RenderDevice>,
    queue: Res<'w, RenderQueue>,
    pipeline_cache: Res<'w, PipelineCache>,
    pipeline: Res<'w, UiMaterialPipeline<SurfaceUiMaterial>>,
    images: Res<'w, RenderAssets<GpuImage>>,
}

fn prepare_surface_bindings(
    published: Res<PublishedSurfaceBindings>,
    gpu: SurfaceBindingResources,
    mut materials: ResMut<RenderAssets<PreparedUiMaterial<SurfaceUiMaterial>>>,
    mut cache: ResMut<SurfaceBindingCache>,
) {
    cache.bindings.retain(|key, _| {
        published.material_parameters.get(&key.material) == Some(&key.parameters)
            && published.valid_images.contains(&key.image)
    });
    cache.parameters.retain(|material, buffer| {
        published.material_parameters.get(material) == Some(&(*buffer.get()).into())
    });
    for (&material_id, desired) in &published.desired {
        let Some(material) = materials.get_mut(material_id) else {
            tracing::trace!(?material_id, "surface material is not prepared yet");
            continue;
        };
        let Some(image) = gpu.images.get(desired.image) else {
            tracing::trace!(?material_id, ?desired.image, "surface image is not prepared yet");
            continue;
        };
        let key = SurfaceBindingCacheKey {
            material: material_id,
            image: desired.image,
            generation: desired.generation,
            parameters: desired.parameters.into(),
        };
        let bind_group = if let Some(bind_group) = cache.bindings.get(&key) {
            bind_group.clone()
        } else {
            // Late image promotion can precede Bevy's material-asset extraction.
            // Build immutable uniforms from the same snapshot used for the cache key.
            // Rotating images share these uniforms until the parameters change.
            let parameters = cache.parameters.entry(material_id).or_insert_with(|| {
                let mut buffer = UniformBuffer::from(desired.parameters);
                buffer.set_label(Some("weld surface parameters"));
                buffer.write_buffer(&gpu.device, &gpu.queue);
                buffer
            });
            let Some(uniform) = parameters.binding() else {
                warn!(?material_id, "surface parameter buffer is unavailable");
                continue;
            };
            let entries = [
                BindGroupEntry {
                    binding: 0,
                    resource: BindingResource::TextureView(&image.texture_view),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: uniform,
                },
            ];
            let layout = gpu
                .pipeline_cache
                .get_bind_group_layout(&gpu.pipeline.ui_layout);
            let bind_group =
                gpu.device
                    .create_bind_group("weld stable surface material", &layout, &entries);
            cache.bindings.insert(key, bind_group.clone());
            bind_group
        };
        material.bind_group = bind_group;
    }
}

#[cfg(all(test, feature = "test-support"))]
pub(super) mod tests {
    use super::*;
    use bevy::render::{
        RenderApp,
        render_resource::{BindGroupId, BufferId},
    };

    pub(crate) fn cache_allocations(app: &App) -> (HashSet<BindGroupId>, HashSet<BufferId>) {
        let cache = app
            .sub_app(RenderApp)
            .world()
            .resource::<SurfaceBindingCache>();
        (
            cache.bindings.values().map(BindGroup::id).collect(),
            cache
                .parameters
                .values()
                .filter_map(|uniform| uniform.buffer().map(|buffer| buffer.id()))
                .collect(),
        )
    }
}
