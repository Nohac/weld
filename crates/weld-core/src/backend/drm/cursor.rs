//! Cursor-theme and client-image normalization for Smithay render elements.

use std::{cell::RefCell, collections::HashMap, fs, rc::Rc, sync::Arc};

use anyhow::{Context, Result, bail};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportMem, Renderer,
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            },
        },
    },
    input::pointer::CursorIcon,
    utils::{Buffer, Logical, Physical, Point, Rectangle, Size, Transform},
};
use tracing::warn;
use xcursor::{
    CursorTheme,
    parser::{Image, parse_xcursor},
};

use crate::{
    cursor::{
        CursorConfiguration, CursorImage,
        raster::{
            CropSource, destination_raster, premultiply_alpha, resample_pixels, scaled_extent,
            swap_red_blue,
        },
    },
    input::InputPosition,
};

const FALLBACK_CURSOR_SIZE: u32 = 24;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ThemeIcon {
    theme: String,
    icon: CursorIcon,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct NormalizedThemeIcon {
    icon: ThemeIcon,
    logical_size: u32,
    scale_bits: u64,
}

#[derive(Debug, Default)]
struct CursorResourceCache {
    theme_images: HashMap<ThemeIcon, Arc<[Image]>>,
    normalized_named: HashMap<NormalizedThemeIcon, NormalizedCursor>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct CursorResources(Rc<RefCell<CursorResourceCache>>);

#[derive(Debug)]
pub(super) struct CursorState {
    configuration: CursorConfiguration,
    image: CursorImage,
    position: InputPosition,
    scale: f64,
    resources: CursorResources,
    normalized: Option<NormalizedCursor>,
}

#[derive(Clone, Debug)]
struct NormalizedCursor {
    buffer: MemoryRenderBuffer,
    source: Rectangle<f64, Logical>,
    logical_size: Size<i32, Logical>,
    hotspot: Point<i32, Physical>,
    plane_eligible: bool,
}

impl CursorState {
    pub(super) fn new(
        configuration: CursorConfiguration,
        scale: f64,
        resources: CursorResources,
    ) -> Result<Self> {
        let mut state = Self {
            configuration,
            image: CursorImage::Named(CursorIcon::Default),
            position: InputPosition::default(),
            scale,
            resources,
            normalized: None,
        };
        state.rebuild()?;
        Ok(state)
    }

    pub(super) fn set_configuration(&mut self, configuration: CursorConfiguration) -> Result<()> {
        if self.configuration != configuration {
            self.configuration = configuration;
            self.rebuild()?;
        }
        Ok(())
    }

    pub(super) fn set_image(&mut self, image: CursorImage) -> Result<()> {
        self.image = image;
        self.rebuild()
    }

    pub(super) fn set_position(&mut self, position: InputPosition) {
        self.position = position;
    }

    pub(super) fn set_scale(&mut self, scale: f64) -> Result<()> {
        if self.scale != scale {
            self.scale = scale;
            self.rebuild()?;
        }
        Ok(())
    }

    pub(super) fn render_element<R>(
        &self,
        renderer: &mut R,
    ) -> Result<Option<MemoryRenderBufferRenderElement<R>>, R::Error>
    where
        R: Renderer + ImportMem,
        R::TextureId: Clone + Send + 'static,
    {
        let Some(cursor) = &self.normalized else {
            return Ok(None);
        };
        let physical_x = (self.position.x * self.scale).round() as i32 - cursor.hotspot.x;
        let physical_y = (self.position.y * self.scale).round() as i32 - cursor.hotspot.y;
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            Point::<f64, Physical>::from((f64::from(physical_x), f64::from(physical_y))),
            &cursor.buffer,
            Some(1.0),
            Some(cursor.source),
            Some(cursor.logical_size),
            if cursor.plane_eligible {
                Kind::Cursor
            } else {
                Kind::Unspecified
            },
        )
        .map(Some)
    }

    fn rebuild(&mut self) -> Result<()> {
        self.normalized = match &self.image {
            CursorImage::Hidden => None,
            CursorImage::Named(icon) => Some(self.normalize_named(*icon)?),
            CursorImage::Image(image) => Some(normalize_client(
                image,
                self.configuration.size(),
                self.scale,
            )?),
        };
        Ok(())
    }

    fn normalize_named(&mut self, icon: CursorIcon) -> Result<NormalizedCursor> {
        let icon_key = ThemeIcon {
            theme: self.configuration.theme().to_owned(),
            icon,
        };
        let logical_size = self.configuration.size();
        let normalized_key = NormalizedThemeIcon {
            icon: icon_key.clone(),
            logical_size,
            scale_bits: self.scale.to_bits(),
        };
        if let Some(cursor) = self
            .resources
            .0
            .borrow()
            .normalized_named
            .get(&normalized_key)
            .cloned()
        {
            return Ok(cursor);
        }
        let cached_images = self
            .resources
            .0
            .borrow()
            .theme_images
            .get(&icon_key)
            .cloned();
        let images = if let Some(images) = cached_images {
            images
        } else {
            let images = load_theme_images(&icon_key).unwrap_or_else(|error| {
                warn!(theme = %icon_key.theme, icon = icon_key.icon.name(), %error, "using the built-in cursor image");
                Arc::from([fallback_image()])
            });
            self.resources
                .0
                .borrow_mut()
                .theme_images
                .insert(icon_key, Arc::clone(&images));
            images
        };
        let physical_size = scaled_extent(logical_size as f64, self.scale)?;
        let image = images
            .iter()
            .min_by_key(|image| image.size.abs_diff(physical_size))
            .context("cursor theme contains no images")?;
        let pixels = resample_pixels(
            &image.pixels_rgba,
            image.width,
            image.height,
            physical_size,
            physical_size,
            CropSource::full(image.width, image.height),
        )?;
        let hotspot = Point::from((
            scale_coordinate(image.xhot, image.width, physical_size),
            scale_coordinate(image.yhot, image.height, physical_size),
        ));
        let normalized = normalized_cursor(
            pixels,
            physical_size,
            physical_size,
            logical_size as f64,
            logical_size as f64,
            hotspot,
            self.scale,
        )?;
        // Named cursor entries remain small in ordinary use: cursor icons are
        // finite and runtime output scale changes use quarter-step values.
        self.resources
            .0
            .borrow_mut()
            .normalized_named
            .insert(normalized_key, normalized.clone());
        Ok(normalized)
    }
}

fn normalize_client(
    image: &weld_client::ClientCursorImage,
    nominal: u32,
    scale: f64,
) -> Result<NormalizedCursor> {
    let raster = destination_raster(image, nominal, scale)?;
    let mut pixels = raster.rgba;
    swap_red_blue(&mut pixels);
    premultiply_alpha(&mut pixels);
    let hotspot = Point::from((raster.hotspot.0 as i32, raster.hotspot.1 as i32));
    normalized_cursor(
        pixels,
        raster.width,
        raster.height,
        f64::from(raster.width) / scale,
        f64::from(raster.height) / scale,
        hotspot,
        scale,
    )
}

fn normalized_cursor(
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    logical_width: f64,
    logical_height: f64,
    hotspot: Point<i32, Physical>,
    scale: f64,
) -> Result<NormalizedCursor> {
    let width_i32 = i32::try_from(width).context("cursor width exceeds i32")?;
    let height_i32 = i32::try_from(height).context("cursor height exceeds i32")?;
    let logical_width_i32 = logical_width.round() as i32;
    let logical_height_i32 = logical_height.round() as i32;
    if logical_width_i32 <= 0 || logical_height_i32 <= 0 {
        bail!("cursor logical dimensions must be positive");
    }
    let plane_eligible = smithay_physical_extent(logical_width_i32, scale, 0) == width_i32
        && smithay_physical_extent(logical_height_i32, scale, 0) == height_i32;
    let buffer = MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        Size::<i32, Buffer>::from((width_i32, height_i32)),
        1,
        Transform::Normal,
        None,
    );
    Ok(NormalizedCursor {
        buffer,
        source: Rectangle::from_size(Size::from((f64::from(width), f64::from(height)))),
        logical_size: Size::from((logical_width_i32, logical_height_i32)),
        hotspot,
        plane_eligible,
    })
}

fn load_theme_images(key: &ThemeIcon) -> Result<Arc<[Image]>> {
    let theme = CursorTheme::load(&key.theme);
    let mut requested_names = std::iter::once(key.icon.name())
        .chain(key.icon.alt_names().iter().copied())
        .chain(std::iter::once(CursorIcon::Default.name()))
        .chain(CursorIcon::Default.alt_names().iter().copied());
    let path = requested_names
        .find_map(|name| theme.load_icon(name))
        .context("cursor theme has neither the requested nor default icon")?;
    let bytes = fs::read(&path)
        .with_context(|| format!("failed to read cursor icon {}", path.display()))?;
    let images = parse_xcursor(&bytes).context("failed to parse cursor icon")?;
    if images.is_empty() {
        bail!("cursor icon contains no frames");
    }
    Ok(images.into())
}

fn fallback_image() -> Image {
    let mut pixels = vec![0_u8; (FALLBACK_CURSOR_SIZE * FALLBACK_CURSOR_SIZE * 4) as usize];
    for y in 0..FALLBACK_CURSOR_SIZE {
        for x in 0..FALLBACK_CURSOR_SIZE {
            let head = y < 17 && x <= y / 2;
            let stem = (5..=8).contains(&x) && (11..=22).contains(&y);
            if !head && !stem {
                continue;
            }
            let border = x == 0
                || y == 0
                || (head && (x == y / 2 || y == 16))
                || (stem && (x == 5 || x == 8 || y == 22));
            let offset = ((y * FALLBACK_CURSOR_SIZE + x) * 4) as usize;
            let color = if border { 24 } else { 245 };
            pixels[offset..offset + 4].copy_from_slice(&[color, color, color, 255]);
        }
    }
    Image {
        size: FALLBACK_CURSOR_SIZE,
        width: FALLBACK_CURSOR_SIZE,
        height: FALLBACK_CURSOR_SIZE,
        xhot: 1,
        yhot: 1,
        delay: 1,
        pixels_rgba: pixels,
        pixels_argb: Vec::new(),
    }
}

fn smithay_physical_extent(logical: i32, scale: f64, physical_location: i32) -> i32 {
    (f64::from(logical) * scale + f64::from(physical_location)).round() as i32 - physical_location
}

fn scale_coordinate(value: u32, source_extent: u32, target_extent: u32) -> i32 {
    (f64::from(value) / f64::from(source_extent) * f64::from(target_extent)).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_raster_extent_matches_supported_fractional_scales() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let extent = scaled_extent(24.0, scale).expect("valid extent");
            for location in [-48, 0, 137] {
                assert_eq!(smithay_physical_extent(24, scale, location), extent as i32);
            }
        }
    }

    #[test]
    fn client_cursor_crop_is_normalized_and_premultiplied() {
        let image = crate::cursor::raster::canonical_cursor(
            &[0, 0, 0, 0, 200, 100, 50, 128, 0, 0, 0, 0, 0, 0, 0, 0],
            2,
            2,
            crate::surface::SurfaceContentView {
                source_x: 1.0,
                source_y: 0.0,
                source_width: 1.0,
                source_height: 1.0,
                logical_width: 1.0,
                logical_height: 1.0,
            },
            (0.0, 0.0),
        )
        .expect("valid canonical cursor");

        let mut normalized = normalize_client(&image, 1, 1.0).expect("valid cursor");
        let mut renderer = normalized.buffer.render();
        renderer
            .draw::<_, std::convert::Infallible>(|pixels| {
                assert_eq!(&pixels[..4], &[100, 50, 25, 128]);
                Ok(Vec::new())
            })
            .expect("infallible cursor inspection");
    }

    #[test]
    fn empty_cursor_source_is_rejected_before_sampling() {
        assert!(
            resample_pixels(&[], 0, 0, 1, 1, CropSource::full(0, 0)).is_err(),
            "an empty client buffer must not reach bilinear coordinate math"
        );
    }
}
