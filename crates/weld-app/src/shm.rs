//! Uploads client SHM snapshots into stable per-layer textures. Content updates
//! share the composition queue and preserve Bevy image identities and draw batches.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};
use bevy::{
    app::App,
    asset::{AssetId, Assets, Handle, RenderAssetUsages},
    ecs::{
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Res, ResMut},
    },
    image::Image,
    render::{
        Render, RenderApp, RenderSystems,
        render_asset::{RenderAssets, prepare_assets},
        render_resource::{DefaultImageSampler, Texture, TextureView},
        texture::GpuImage,
    },
    ui_render::ImageNodeBindGroups,
};
use weld_client::{Extent, SurfaceLayerId};
use weld_core::surface::SurfaceId;

struct UploadedImage {
    handle: Handle<Image>,
    texture: wgpu::Texture,
}

#[derive(Resource, Default)]
struct UploadedTextures(HashMap<AssetId<Image>, (Texture, TextureView)>);

/// Runs after renderer startup so externally supplied textures survive Bevy's
/// initial asset reset. Stable entries never mutate RenderAssets' change tick.
fn install_uploaded_images(
    uploaded: Res<UploadedTextures>,
    sampler: Res<DefaultImageSampler>,
    mut images: ResMut<RenderAssets<GpuImage>>,
) {
    for (&id, (texture, view)) in &uploaded.0 {
        if images
            .get(id)
            .is_some_and(|image| image.texture.id() == texture.id())
        {
            continue;
        }
        images.insert(
            id,
            GpuImage {
                texture: texture.clone(),
                texture_view: view.clone(),
                sampler: (**sampler).clone(),
                texture_descriptor: wgpu::TextureDescriptor {
                    label: Some("Weld retained SHM image"),
                    size: texture.size(),
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                },
                texture_view_descriptor: None,
                had_data: false,
            },
        );
    }
}

pub(crate) struct ShmImages {
    device: wgpu::Device,
    queue: wgpu::Queue,
    // Keep one texture per layer across DMA-BUF transitions: it remains the
    // fallback until promotion completes and is reusable if SHM resumes.
    images: HashMap<SurfaceId, HashMap<SurfaceLayerId, UploadedImage>>,
}

impl ShmImages {
    pub(crate) fn new(app: &mut App, device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Self> {
        app.get_sub_app_mut(RenderApp)
            .context("SHM upload requires RenderApp")?
            .init_resource::<UploadedTextures>()
            .add_systems(
                Render,
                // UI materials are prepared after Bevy's image assets. Install
                // native SHM images before that dependency so a resized, idle
                // client cannot leave its material waiting for another frame.
                install_uploaded_images
                    .in_set(RenderSystems::PrepareAssets)
                    .before(prepare_assets::<GpuImage>),
            );
        Ok(Self {
            device: device.clone(),
            queue: queue.clone(),
            images: HashMap::new(),
        })
    }

    pub(crate) fn upload(
        &mut self,
        app: &mut App,
        surface: SurfaceId,
        layer: SurfaceLayerId,
        extent: Extent,
        pixels: &[u8],
    ) -> Result<Handle<Image>> {
        let row_bytes = extent.width.checked_mul(4).context("SHM row overflow")?;
        ensure!(
            extent.width > 0
                && extent.height > 0
                && extent.width <= self.device.limits().max_texture_dimension_2d
                && extent.height <= self.device.limits().max_texture_dimension_2d
                && u64::from(row_bytes) * u64::from(extent.height) == pixels.len() as u64,
            "invalid SHM upload extent"
        );
        let size = wgpu::Extent3d {
            width: extent.width,
            height: extent.height,
            depth_or_array_layers: 1,
        };
        if self
            .images
            .get(&surface)
            .and_then(|layers| layers.get(&layer))
            .is_none_or(|image| image.texture.size() != size)
        {
            let image = self.create(app, size)?;
            if let Some(previous) = self.images.entry(surface).or_default().insert(layer, image) {
                retire(app, &previous.handle);
            }
        }
        let image = self
            .images
            .get(&surface)
            .and_then(|layers| layers.get(&layer))
            .context("SHM image is unavailable")?;
        // write_texture owns its staging copy before returning. The retained host
        // snapshot can be released immediately; the next composition submission
        // orders this write after earlier draws and before sampling the new pixels.
        self.queue.write_texture(
            image.texture.as_image_copy(),
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row_bytes),
                rows_per_image: Some(extent.height),
            },
            size,
        );
        Ok(image.handle.clone())
    }

    fn create(&self, app: &mut App, size: wgpu::Extent3d) -> Result<UploadedImage> {
        let descriptor = wgpu::TextureDescriptor {
            label: Some("Weld retained SHM image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        };
        let texture = self.device.create_texture(&descriptor);
        let handle = app
            .world_mut()
            .get_resource_mut::<Assets<Image>>()
            .context("SHM upload requires image assets")?
            .add(Image::new_uninit(
                size,
                descriptor.dimension,
                descriptor.format,
                RenderAssetUsages::MAIN_WORLD,
            ));
        app.sub_app_mut(RenderApp)
            .world_mut()
            .resource_mut::<UploadedTextures>()
            .0
            .insert(
                handle.id(),
                (
                    texture.clone().into(),
                    texture.create_view(&Default::default()).into(),
                ),
            );
        Ok(UploadedImage { handle, texture })
    }

    pub(crate) fn retain_layers(
        &mut self,
        app: &mut App,
        surface: SurfaceId,
        layers: &HashSet<SurfaceLayerId>,
    ) {
        let Some(uploaded) = self.images.get_mut(&surface) else {
            return;
        };
        uploaded.retain(|layer, image| {
            if layers.contains(layer) {
                return true;
            }
            retire(app, &image.handle);
            false
        });
        if uploaded.is_empty() {
            self.images.remove(&surface);
        }
    }

    pub(crate) fn remove_surface(&mut self, app: &mut App, surface: SurfaceId) {
        if let Some(images) = self.images.remove(&surface) {
            for image in images.into_values() {
                retire(app, &image.handle);
            }
        }
    }
}

fn retire(app: &mut App, handle: &Handle<Image>) {
    if let Some(render) = app.get_sub_app_mut(RenderApp) {
        let world = render.world_mut();
        if let Some(mut uploaded) = world.get_resource_mut::<UploadedTextures>() {
            uploaded.0.remove(&handle.id());
        }
        if let Some(mut bindings) = world.get_resource_mut::<ImageNodeBindGroups>() {
            bindings.values.remove(&handle.id());
        }
        if let Some(mut images) = world.get_resource_mut::<RenderAssets<GpuImage>>() {
            images.remove(handle.id());
        }
    }
    if let Some(mut images) = app.world_mut().get_resource_mut::<Assets<Image>>() {
        images.remove(handle.id());
    }
}
