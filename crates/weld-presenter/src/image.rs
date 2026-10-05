use bevy_asset::AssetId;
use bevy_image::Image;
use bevy_render::{render_asset::RenderAssets, texture::GpuImage};
use bevy_ui_render::ImageNodeBindGroups;

/// Publish a native import after its producer synchronization is satisfied.
/// Stable textures retain their existing render assets and UI bindings.
pub fn publish_image(
    images: &mut RenderAssets<GpuImage>,
    bindings: Option<&mut ImageNodeBindGroups>,
    id: AssetId<Image>,
    image: GpuImage,
) {
    if images.get(id).is_some_and(|current| {
        current.texture.id() == image.texture.id()
            && current.texture_view.id() == image.texture_view.id()
            && current.sampler.id() == image.sampler.id()
    }) {
        return;
    }
    if let Some(bindings) = bindings {
        bindings.values.remove(&id);
    }
    images.insert(id, image);
}

/// Retire a native image and every ordinary UI binding that samples it.
pub fn retire_image(
    images: &mut RenderAssets<GpuImage>,
    bindings: Option<&mut ImageNodeBindGroups>,
    id: AssetId<Image>,
) {
    if let Some(bindings) = bindings {
        bindings.values.remove(&id);
    }
    images.remove(id);
}
