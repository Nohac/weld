//! Render-world selection and caching for stable surface-layer materials.

use std::collections::{HashMap, HashSet};

use bevy::{
    app::{App, SubApp},
    asset::{AssetId, Assets},
    ecs::{resource::Resource, schedule::IntoScheduleConfigs},
    image::Image,
    math::UVec2,
    render::{
        Render, RenderSystems,
        render_asset::RenderAssets,
        render_resource::{BindGroup, BindGroupEntry, BindingResource, PipelineCache},
        renderer::RenderDevice,
        texture::GpuImage,
    },
    ui_render::{PreparedUiMaterial, UiMaterialPipeline},
};
use tracing::warn;

use super::{
    MaterialSelectorRegistry, SurfaceImageEncoding, SurfaceParameterKey, SurfaceRegistry,
    SurfaceUiMaterial, encoding_flag,
};

#[derive(Clone, Copy)]
struct PublishedSurfaceBinding {
    image: AssetId<Image>,
    generation: u64,
    parameters: SurfaceParameterKey,
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
struct SurfaceBindingCache(HashMap<SurfaceBindingCacheKey, BindGroup>);

pub(super) fn configure_render_app(render_app: &mut SubApp) {
    render_app
        .init_resource::<PublishedSurfaceBindings>()
        .init_resource::<SurfaceBindingCache>()
        .add_systems(
            Render,
            prepare_surface_bindings.in_set(RenderSystems::PrepareBindGroups),
        );
}

pub(crate) fn publish_surface_bindings(app: &mut App, mut valid_images: HashSet<AssetId<Image>>) {
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
        } else if buffer.encoding == SurfaceImageEncoding::LinearStraight {
            (
                buffer.image.id(),
                buffer.generation,
                SurfaceImageEncoding::LinearStraight,
                false,
            )
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
        let parameters = material.parameters.into();
        if let Some(mut materials) = app
            .world_mut()
            .get_resource_mut::<Assets<SurfaceUiMaterial>>()
            && materials.get(material_id) != Some(&material)
            && let Err(error) = materials.insert(material_id, material)
        {
            warn!(%error, ?material_id, "could not refresh a surface material");
        }
        if let Some(mut selectors) = app
            .world_mut()
            .get_resource_mut::<MaterialSelectorRegistry>()
            && let Some(current) = selectors.0.get_mut(&material_id)
            && current.surface == selector.surface
            && current.layer == selector.layer
        {
            current.parameters = parameters;
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
            .insert(material_id, parameters);
    }
    if let Some(render_app) = app.get_sub_app_mut(bevy::render::RenderApp) {
        render_app.world_mut().insert_resource(published);
    }
}

fn prepare_surface_bindings(
    published: bevy::ecs::system::Res<PublishedSurfaceBindings>,
    render_device: bevy::ecs::system::Res<RenderDevice>,
    pipeline_cache: bevy::ecs::system::Res<PipelineCache>,
    pipeline: bevy::ecs::system::Res<UiMaterialPipeline<SurfaceUiMaterial>>,
    gpu_images: bevy::ecs::system::Res<RenderAssets<GpuImage>>,
    mut materials: bevy::ecs::system::ResMut<RenderAssets<PreparedUiMaterial<SurfaceUiMaterial>>>,
    mut cache: bevy::ecs::system::ResMut<SurfaceBindingCache>,
) {
    cache.0.retain(|key, _| {
        published.material_parameters.get(&key.material) == Some(&key.parameters)
            && published.valid_images.contains(&key.image)
    });
    for (&material_id, desired) in &published.desired {
        let Some(material) = materials.get_mut(material_id) else {
            tracing::trace!(?material_id, "surface material is not prepared yet");
            continue;
        };
        let Some(image) = gpu_images.get(desired.image) else {
            tracing::trace!(?material_id, ?desired.image, "surface image is not prepared yet");
            continue;
        };
        let key = SurfaceBindingCacheKey {
            material: material_id,
            image: desired.image,
            generation: desired.generation,
            parameters: desired.parameters,
        };
        let bind_group = if let Some(bind_group) = cache.0.get(&key) {
            bind_group.clone()
        } else {
            let entries = material
                .bindings
                .iter()
                .map(|(binding, resource)| BindGroupEntry {
                    binding: *binding,
                    resource: if *binding == 0 {
                        BindingResource::TextureView(&image.texture_view)
                    } else {
                        resource.get_binding()
                    },
                })
                .collect::<Vec<_>>();
            let layout = pipeline_cache.get_bind_group_layout(&pipeline.ui_layout);
            let bind_group =
                render_device.create_bind_group("weld stable surface material", &layout, &entries);
            cache.0.insert(key, bind_group.clone());
            bind_group
        };
        material.bind_group = bind_group;
    }
}
