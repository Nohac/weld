//! SVG assets rendered by resvg into cached, physical-pixel-sized UI images.

use std::collections::{HashMap, HashSet, hash_map::Entry};

use anyhow::{Context, Result, ensure};
use bevy::{
    asset::{AssetId, AssetLoader, LoadContext, RenderAssetUsages, io::Reader},
    image::TRANSPARENT_IMAGE_HANDLE,
    prelude::{
        App, Asset, AssetApp, AssetEvent, Assets, Component, ComputedNode,
        ComputedUiRenderTargetInfo, DetectChanges, Handle, Image, ImageNode, IntoScheduleConfigs,
        Local, MessageReader, Plugin, PostUpdate, Query, Ref, Res, ResMut, Resource, TypePath,
        UVec2, Vec2,
    },
    render::{
        render_resource::{Extent3d, TextureDimension, TextureFormat},
        renderer::RenderDevice,
    },
    ui::{
        ContentSize, NodeMeasure, UiSystems,
        widget::{ImageMeasure, update_image_content_size_system},
    },
};
use resvg::{
    tiny_skia::{Pixmap, Transform},
    usvg,
};

const MAX_RASTER_PIXELS: u64 = 16_777_216;

/// Parsed SVG artwork. Explicit fills, strokes and gradients retain their colors.
#[derive(Asset, TypePath)]
pub struct Svg(usvg::Tree);

impl Svg {
    /// Parses standalone SVG artwork using its own colors.
    pub fn parse(source: &str) -> Result<Self> {
        Self::parse_with_options(source, &usvg::Options::default())
    }

    /// Parses an icon with inherited `currentColor` set to white for UI tinting.
    pub fn parse_icon(source: &str) -> Result<Self> {
        Self::parse_with_options(
            source,
            &usvg::Options {
                style_sheet: Some("svg { color: white; }".into()),
                ..Default::default()
            },
        )
    }

    fn parse_with_options(source: &str, options: &usvg::Options<'_>) -> Result<Self> {
        usvg::Tree::from_str(source, options)
            .map(Self)
            .context("parse SVG")
    }

    fn size(&self) -> Vec2 {
        Vec2::new(self.0.size().width(), self.0.size().height())
    }

    fn rasterize(&self, extent: UVec2, max_dimension: u32) -> Result<Image> {
        ensure!(
            extent.max_element() <= max_dimension,
            "SVG raster exceeds the device texture limit"
        );
        // Bound CPU raster memory independently of the graphics adapter's texture limits.
        ensure!(
            u64::from(extent.x) * u64::from(extent.y) <= MAX_RASTER_PIXELS,
            "SVG raster exceeds 16 megapixels"
        );
        let mut pixmap = Pixmap::new(extent.x, extent.y).context("allocate SVG raster")?;
        let scale = extent.as_vec2() / self.size();
        resvg::render(
            &self.0,
            Transform::from_scale(scale.x, scale.y),
            &mut pixmap.as_mut(),
        );
        let mut pixels = pixmap.take();
        // tiny-skia produces premultiplied pixels; Bevy's image shader expects straight alpha.
        for pixel in pixels.as_chunks_mut::<4>().0 {
            let alpha = u16::from(pixel[3]);
            for channel in &mut pixel[..3] {
                *channel = (u16::from(*channel) * 255 + alpha / 2)
                    .checked_div(alpha)
                    .unwrap_or(0) as u8;
            }
        }
        Ok(Image::new(
            Extent3d {
                width: extent.x,
                height: extent.y,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            pixels,
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::RENDER_WORLD,
        ))
    }
}

/// Rasterizes an SVG to the node's physical size; [`ImageNode::color`] supplies tint.
#[derive(Component, Clone)]
#[require(ImageNode = ImageNode::new(TRANSPARENT_IMAGE_HANDLE))]
pub struct SvgImage(pub Handle<Svg>);

#[derive(Default, TypePath)]
struct SvgLoader;

impl AssetLoader for SvgLoader {
    type Asset = Svg;
    type Settings = ();
    type Error = anyhow::Error;

    async fn load(&self, reader: &mut dyn Reader, _: &(), _: &mut LoadContext<'_>) -> Result<Svg> {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .await
            .context("read SVG asset")?;
        Svg::parse(std::str::from_utf8(&bytes).context("SVG asset UTF-8")?)
    }

    fn extensions(&self) -> &[&str] {
        &["svg"]
    }
}

type RasterKey = (AssetId<Svg>, UVec2);

#[derive(Resource, Default)]
struct SvgRasters(HashMap<RasterKey, Option<Handle<Image>>>);

/// Installs SVG asset loading, intrinsic sizing and shared raster caching.
pub struct SvgPlugin;

impl Plugin for SvgPlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<Svg>()
            .init_asset_loader::<SvgLoader>()
            .init_resource::<SvgRasters>()
            .add_systems(
                PostUpdate,
                (
                    measure_svgs
                        .in_set(UiSystems::Content)
                        .after(update_image_content_size_system),
                    rasterize_svgs.after(UiSystems::Layout),
                ),
            );
    }
}

type SvgMeasurements<'w, 's> = Query<
    'w,
    's,
    (
        Ref<'static, SvgImage>,
        Ref<'static, ImageNode>,
        Ref<'static, ComputedUiRenderTargetInfo>,
        &'static mut ContentSize,
    ),
>;

fn measure_svgs(assets: Res<Assets<Svg>>, mut nodes: SvgMeasurements) {
    for (handle, image, target, mut measure) in &mut nodes {
        if let Some(svg) = assets.get(&handle.0)
            && (assets.is_changed()
                || handle.is_changed()
                || image.is_changed()
                || target.is_changed()
                || measure.is_added())
        {
            measure.set(NodeMeasure::Image(ImageMeasure {
                size: svg.size() * target.scale_factor(),
                visual_box: image.visual_box,
            }));
        }
    }
}

fn rasterize_svgs(
    assets: Res<Assets<Svg>>,
    mut events: MessageReader<AssetEvent<Svg>>,
    mut images: ResMut<Assets<Image>>,
    mut cache: ResMut<SvgRasters>,
    mut nodes: Query<(&SvgImage, &ComputedNode, &mut ImageNode)>,
    mut active: Local<HashSet<RasterKey>>,
    device: Option<Res<RenderDevice>>,
) {
    let max_dimension = device
        .as_ref()
        .map_or(u32::MAX, |device| device.limits().max_texture_dimension_2d);
    for event in events.read() {
        if let AssetEvent::Modified { id } | AssetEvent::Removed { id } = event {
            cache.0.retain(|(asset, _), _| asset != id);
        }
    }
    active.clear();
    for (handle, node, mut image) in &mut nodes {
        let Some(svg) = assets.get(&handle.0) else {
            if image.image != TRANSPARENT_IMAGE_HANDLE {
                image.image = TRANSPARENT_IMAGE_HANDLE;
            }
            continue;
        };
        let size = node.size();
        if !size.is_finite() || size.min_element() <= 0.0 {
            continue;
        }
        let extent = size.round().as_uvec2().max(UVec2::ONE);
        let key = (handle.0.id(), extent);
        active.insert(key);
        let raster = match cache.0.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(match svg.rasterize(extent, max_dimension) {
                Ok(raster) => Some(images.add(raster)),
                Err(error) => {
                    tracing::warn!(asset = ?key.0, ?extent, %error, "SVG rasterization failed");
                    None
                }
            }),
        };
        let expected = raster.clone().unwrap_or(TRANSPARENT_IMAGE_HANDLE);
        if image.image != expected {
            image.image = expected;
        }
    }
    cache.0.retain(|key, _| active.contains(key));
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::app::Update;

    const ART: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="12"><rect width="12" height="12" fill="red" fill-opacity="0.5"/><rect x="12" width="12" height="12" fill="blue"/></svg>"#;

    #[test]
    fn image_pixels_preserve_svg_colors_and_convert_to_straight_alpha() {
        let image = Svg::parse(ART)
            .expect("SVG")
            .rasterize(UVec2::new(48, 24), 4096)
            .expect("raster");
        let pixels = image.data.expect("pixels");
        assert_eq!(&pixels[..4], &[255, 0, 0, 128]);
        assert_eq!(&pixels[32 * 4..33 * 4], &[0, 0, 255, 255]);
        assert_eq!(
            image.texture_descriptor.format,
            TextureFormat::Rgba8UnormSrgb
        );
    }

    #[test]
    fn icon_current_color_is_tintable_white() {
        let svg = Svg::parse_icon(r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="currentColor"/></svg>"#).expect("icon");
        let image = svg.rasterize(UVec2::splat(8), 4096).expect("raster");
        assert_eq!(&image.data.expect("pixels")[..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn oversized_rasters_fail_before_allocating_pixels() {
        let svg = Svg::parse(ART).expect("SVG");
        assert!(svg.rasterize(UVec2::new(4097, 1), 4096).is_err());
        assert!(svg.rasterize(UVec2::splat(8192), 16384).is_err());
    }

    #[test]
    fn raster_cache_tracks_size_asset_changes_and_live_consumers() {
        let mut app = App::new();
        app.init_resource::<Assets<Svg>>()
            .init_resource::<Assets<Image>>()
            .init_resource::<SvgRasters>()
            .add_message::<AssetEvent<Svg>>()
            .add_systems(Update, rasterize_svgs);
        let asset = app
            .world_mut()
            .resource_mut::<Assets<Svg>>()
            .add(Svg::parse(ART).expect("SVG"));
        let node = ComputedNode {
            size: Vec2::new(24., 12.),
            ..Default::default()
        };
        let first = app
            .world_mut()
            .spawn((SvgImage(asset.clone()), node.clone()))
            .id();
        let second = app.world_mut().spawn((SvgImage(asset.clone()), node)).id();
        app.update();
        let original = app
            .world()
            .get::<ImageNode>(first)
            .expect("image")
            .image
            .clone();
        assert_eq!(
            app.world().get::<ImageNode>(second).expect("image").image,
            original
        );
        app.update();
        assert_eq!(
            app.world().get::<ImageNode>(first).expect("image").image,
            original
        );
        app.world_mut()
            .get_mut::<ComputedNode>(first)
            .expect("layout")
            .size *= 1.5;
        app.update();
        let scaled = app
            .world()
            .get::<ImageNode>(first)
            .expect("image")
            .image
            .clone();
        assert_ne!(scaled, original);
        assert_eq!(
            app.world()
                .resource::<Assets<Image>>()
                .get(&scaled)
                .expect("raster")
                .size(),
            UVec2::new(36, 18)
        );
        app.world_mut()
            .write_message(AssetEvent::<Svg>::Modified { id: asset.id() });
        app.update();
        assert_ne!(
            app.world().get::<ImageNode>(first).expect("image").image,
            scaled
        );
        app.world_mut().despawn(first);
        app.world_mut().despawn(second);
        app.update();
        assert!(app.world().resource::<SvgRasters>().0.is_empty());
    }
}
