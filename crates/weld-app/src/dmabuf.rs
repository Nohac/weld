//! Bevy image registration for core-owned DMA-BUF leases.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use bevy::{
    app::App,
    asset::{Assets, Handle, RenderAssetUsages},
    image::Image,
    render::{
        RenderApp,
        render_asset::RenderAssets,
        render_resource::{DefaultImageSampler, Texture, TextureView},
        texture::GpuImage,
    },
};
use tracing::warn;
use weld_core::{
    dmabuf::{
        DmabufContext, DmabufManager, ImportId, ImportedImageRegistry, PendingDmabufFrame,
        PromotionImage,
    },
    surface::{SurfaceId, SurfaceLayerId},
};

use crate::surface::{
    SurfaceImageEncoding, SurfaceRenderImage, promote_dmabuf_sources, referenced_dmabuf_ids,
    reject_dmabuf_sources,
};

pub(crate) struct DmabufImporter {
    manager: DmabufManager,
    handles: HashMap<ImportId, Handle<Image>>,
    installed: HashSet<ImportId>,
}

impl DmabufImporter {
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        context: &DmabufContext,
    ) -> Result<Option<Self>> {
        Ok(context.create_manager(device, queue)?.map(|manager| Self {
            manager,
            handles: HashMap::new(),
            installed: HashSet::new(),
        }))
    }

    pub(crate) fn import(
        &mut self,
        app: &mut App,
        surface: SurfaceId,
        layer: SurfaceLayerId,
        frame: PendingDmabufFrame,
        opaque: bool,
    ) -> Result<SurfaceRenderImage> {
        let staged = self.manager.stage(surface, layer, frame)?;
        let handle = if let Some(handle) = self.handles.get(&staged.id) {
            handle.clone()
        } else {
            let image = Image::new_uninit(
                staged.extent,
                wgpu::TextureDimension::D2,
                staged.format,
                RenderAssetUsages::MAIN_WORLD,
            );
            let handle = app
                .world_mut()
                .get_resource_mut::<Assets<Image>>()
                .context("Bevy image assets are unavailable")?
                .add(image);
            self.handles.insert(staged.id, handle.clone());
            handle
        };
        Ok(SurfaceRenderImage {
            import: staged.id,
            image: handle,
            encoding: if opaque {
                SurfaceImageEncoding::EncodedOpaque
            } else {
                SurfaceImageEncoding::EncodedPremultiplied
            },
            y_inverted: staged.y_inverted,
            promoted: false,
        })
    }

    pub(crate) fn prepare_render(&mut self, app: &mut App) -> Result<()> {
        let referenced = referenced_dmabuf_ids(app.world());
        let Self {
            manager,
            handles,
            installed,
        } = self;
        let mut registry = BevyImageRegistry {
            app,
            handles,
            installed,
        };
        match manager.prepare_render(&referenced, &mut registry) {
            Ok(promoted) => promote_dmabuf_sources(registry.app.world_mut(), &promoted),
            Err(error) => {
                reject_dmabuf_sources(registry.app.world_mut(), &referenced);
                warn!(%error, "kept previous client buffers after DMA-BUF promotion failed");
            }
        }
        Ok(())
    }

    pub(crate) fn finish_render(&mut self, app: &mut App) -> Result<()> {
        let Self {
            manager,
            handles,
            installed,
        } = self;
        let mut registry = BevyImageRegistry {
            app,
            handles,
            installed,
        };
        manager.finish_render(&mut registry)
    }

    pub(crate) fn installed_image_ids(&self) -> HashSet<bevy::asset::AssetId<Image>> {
        self.installed
            .iter()
            .filter_map(|id| self.handles.get(id).map(|handle| handle.id()))
            .collect()
    }

    pub(crate) fn retain_surface_layers(
        &mut self,
        surface: SurfaceId,
        retained: &HashSet<SurfaceLayerId>,
    ) {
        self.manager.retain_surface_layers(surface, retained);
    }

    pub(crate) fn remove_surface(&mut self, surface: SurfaceId) {
        self.manager.remove_surface(surface);
    }

    pub(crate) fn remove_layer(&mut self, surface: SurfaceId, layer: SurfaceLayerId) {
        self.manager.remove_layer(surface, layer);
    }
}

struct BevyImageRegistry<'a> {
    app: &'a mut App,
    handles: &'a mut HashMap<ImportId, Handle<Image>>,
    installed: &'a mut HashSet<ImportId>,
}

impl ImportedImageRegistry for BevyImageRegistry<'_> {
    fn install(&mut self, images: &[PromotionImage]) -> Result<()> {
        let render_app = self
            .app
            .get_sub_app_mut(RenderApp)
            .context("Bevy RenderApp is unavailable")?;
        let sampler = render_app
            .world()
            .get_resource::<DefaultImageSampler>()
            .context("Bevy default image sampler is unavailable")?
            .clone();
        let mut gpu_images = render_app
            .world_mut()
            .get_resource_mut::<RenderAssets<GpuImage>>()
            .context("Bevy GPU image assets are unavailable")?;
        for image in images {
            if self.installed.contains(&image.id) {
                continue;
            }
            let handle = self
                .handles
                .get(&image.id)
                .context("DMA-BUF promotion has no Bevy placeholder")?;
            gpu_images.insert(
                handle.id(),
                GpuImage {
                    texture: Texture::from(image.texture.clone()),
                    texture_view: TextureView::from(image.view.clone()),
                    sampler: (*sampler).clone(),
                    texture_descriptor: wgpu::TextureDescriptor {
                        label: Some("weld direct client DMA-BUF"),
                        size: image.texture.size(),
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: image.format,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    },
                    texture_view_descriptor: None,
                    had_data: false,
                },
            );
            self.installed.insert(image.id);
        }
        Ok(())
    }

    fn prune(&mut self, images: &[ImportId]) {
        for id in images {
            self.installed.remove(id);
        }
        let handles = images
            .iter()
            .filter_map(|id| self.handles.remove(id))
            .collect::<Vec<_>>();
        if let Some(render_app) = self.app.get_sub_app_mut(RenderApp)
            && let Some(mut gpu_images) = render_app
                .world_mut()
                .get_resource_mut::<RenderAssets<GpuImage>>()
        {
            for handle in &handles {
                gpu_images.remove(handle.id());
            }
        }
        if let Some(mut assets) = self.app.world_mut().get_resource_mut::<Assets<Image>>() {
            for handle in handles {
                assets.remove(handle.id());
            }
        }
    }
}
