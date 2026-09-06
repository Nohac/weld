//! Shared bounded cursor raster normalization for nested and DRM presentation.

use anyhow::{Result, bail, ensure};
use weld_client::{ClientCursorImage, SurfaceContentView};

use super::unpremultiply_alpha;

/// Convert a committed cursor viewport to one bounded, lossless wire raster.
pub(crate) fn canonical_cursor(
    bgra: &[u8],
    width: u32,
    height: u32,
    view: SurfaceContentView,
    hotspot: (f32, f32),
) -> Result<ClientCursorImage> {
    ensure!(
        [
            view.source_x,
            view.source_y,
            view.source_width,
            view.source_height,
            view.logical_width,
            view.logical_height,
            hotspot.0,
            hotspot.1
        ]
        .iter()
        .all(|v| v.is_finite())
            && view.source_width > 0.0
            && view.source_height > 0.0
            && view.logical_width > 0.0
            && view.logical_height > 0.0,
        "invalid cursor viewport"
    );
    let longest = view.source_width.max(view.source_height).min(128.0);
    let ratio = longest / view.logical_width.max(view.logical_height);
    let target_width = (view.logical_width * ratio).round().clamp(1.0, 128.0) as u32;
    let target_height = (view.logical_height * ratio).round().clamp(1.0, 128.0) as u32;
    let mut pixels = if width == target_width
        && height == target_height
        && view.source_x == 0.0
        && view.source_y == 0.0
        && view.source_width == width as f32
        && view.source_height == height as f32
    {
        bgra.to_vec()
    } else {
        let mut premultiplied = bgra.to_vec();
        premultiply_alpha(&mut premultiplied);
        let mut cropped = resample_pixels(
            &premultiplied,
            width,
            height,
            target_width,
            target_height,
            CropSource {
                x: f64::from(view.source_x),
                y: f64::from(view.source_y),
                width: f64::from(view.source_width),
                height: f64::from(view.source_height),
            },
        )?;
        unpremultiply_alpha(&mut cropped);
        cropped
    };
    swap_red_blue(&mut pixels);
    let hotspot = (
        (hotspot.0 / view.logical_width * target_width as f32)
            .round()
            .clamp(0.0, (target_width - 1) as f32) as u16,
        (hotspot.1 / view.logical_height * target_height as f32)
            .round()
            .clamp(0.0, (target_height - 1) as f32) as u16,
    );
    Ok(ClientCursorImage::new(
        u16::try_from(target_width)?,
        u16::try_from(target_height)?,
        hotspot,
        pixels,
    )?)
}

pub(crate) struct CursorRaster {
    pub(crate) rgba: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) hotspot: (u32, u32),
}

/// Scale to fit, not merely clamp: even a small sprite uses the local nominal size.
pub(crate) fn destination_raster(
    image: &ClientCursorImage,
    nominal: u32,
    scale: f64,
) -> Result<CursorRaster> {
    let longest = scaled_extent(f64::from(nominal), scale)?;
    ensure!(
        longest <= 512,
        "displayed cursor exceeds the 512-pixel raster bound"
    );
    let ratio = f64::from(longest) / f64::from(image.width().max(image.height()));
    let width = (f64::from(image.width()) * ratio).round().max(1.0) as u32;
    let height = (f64::from(image.height()) * ratio).round().max(1.0) as u32;
    let mut pixels = image.rgba().to_vec();
    premultiply_alpha(&mut pixels);
    let mut rgba = resample_pixels(
        &pixels,
        u32::from(image.width()),
        u32::from(image.height()),
        width,
        height,
        CropSource::full(u32::from(image.width()), u32::from(image.height())),
    )?;
    unpremultiply_alpha(&mut rgba);
    let hotspot = (
        ((f64::from(image.hotspot().0) * ratio).round() as u32).min(width - 1),
        ((f64::from(image.hotspot().1) * ratio).round() as u32).min(height - 1),
    );
    Ok(CursorRaster {
        rgba,
        width,
        height,
        hotspot,
    })
}

pub(crate) fn swap_red_blue(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CropSource {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

impl CropSource {
    pub(crate) fn full(width: u32, height: u32) -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width: f64::from(width),
            height: f64::from(height),
        }
    }

    fn coordinates(&self, x: u32, y: u32, target_width: u32, target_height: u32) -> (f64, f64) {
        (
            self.x + (f64::from(x) + 0.5) / f64::from(target_width) * self.width,
            self.y + (f64::from(y) + 0.5) / f64::from(target_height) * self.height,
        )
    }
}

pub(crate) fn resample_pixels(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    width: u32,
    height: u32,
    region: CropSource,
) -> Result<Vec<u8>> {
    let required = source_width as usize * source_height as usize * 4;
    if source_width == 0
        || source_height == 0
        || source.len() < required
        || width == 0
        || height == 0
    {
        bail!("invalid cursor pixel extent");
    }
    let mut output = vec![0; width as usize * height as usize * 4];
    for y in 0..height {
        for x in 0..width {
            let (source_x, source_y) = region.coordinates(x, y, width, height);
            let output_offset = (y as usize * width as usize + x as usize) * 4;
            sample_bilinear(
                source,
                source_width,
                source_height,
                source_x - 0.5,
                source_y - 0.5,
                &mut output[output_offset..output_offset + 4],
            );
        }
    }
    Ok(output)
}

fn sample_bilinear(source: &[u8], width: u32, height: u32, x: f64, y: f64, output: &mut [u8]) {
    let x = x.clamp(0.0, f64::from(width - 1));
    let y = y.clamp(0.0, f64::from(height - 1));
    let x0 = x.floor().clamp(0.0, f64::from(width - 1)) as u32;
    let y0 = y.floor().clamp(0.0, f64::from(height - 1)) as u32;
    let x1 = x0.saturating_add(1).min(width - 1);
    let y1 = y0.saturating_add(1).min(height - 1);
    let x_weight = (x - x.floor()).clamp(0.0, 1.0);
    let y_weight = (y - y.floor()).clamp(0.0, 1.0);
    for (channel, output) in output.iter_mut().enumerate().take(4) {
        let sample = |sample_x: u32, sample_y: u32| {
            f64::from(
                source[(sample_y as usize * width as usize + sample_x as usize) * 4 + channel],
            )
        };
        let top = sample(x0, y0) * (1.0 - x_weight) + sample(x1, y0) * x_weight;
        let bottom = sample(x0, y1) * (1.0 - x_weight) + sample(x1, y1) * x_weight;
        *output = (top * (1.0 - y_weight) + bottom * y_weight).round() as u8;
    }
}

pub(crate) fn premultiply_alpha(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
    }
}

pub(crate) fn scaled_extent(logical: f64, scale: f64) -> Result<u32> {
    let physical = (logical * scale).round();
    if !physical.is_finite() || physical < 1.0 || physical > f64::from(u32::MAX) {
        bail!("cursor extent is outside the supported range");
    }
    Ok(physical as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upscaling_clamps_edges_before_interpolating() {
        let pixels = resample_pixels(
            &[255, 0, 0, 255, 0, 0, 255, 255],
            2,
            1,
            8,
            4,
            CropSource::full(2, 1),
        )
        .expect("upscale");
        assert_eq!(&pixels[..4], &[255, 0, 0, 255]);
        assert_eq!(&pixels[28..32], &[0, 0, 255, 255]);
    }

    #[test]
    fn destination_size_is_policy_owned_and_scales_both_small_and_large_sprites() {
        for side in [8, 32, 128] {
            let image = ClientCursorImage::new(
                side,
                side / 2,
                (side / 2, side / 4),
                vec![255; usize::from(side) * usize::from(side / 2) * 4],
            )
            .expect("image");
            for scale in [1.0, 1.25, 2.0] {
                let raster = destination_raster(&image, 24, scale).expect("raster");
                assert_eq!(raster.width, (24.0 * scale) as u32);
                assert_eq!(raster.height, (12.0 * scale) as u32);
                assert_eq!(raster.hotspot.0, raster.width / 2);
            }
        }
        let image = ClientCursorImage::new(1, 1, (0, 0), vec![255; 4]).expect("image");
        assert!(destination_raster(&image, u32::MAX, 1.0).is_err());
        assert!(destination_raster(&image, 24, f64::NAN).is_err());
    }

    #[test]
    fn canonical_cursor_preserves_small_pixels_and_bounds_large_viewports() {
        let view = SurfaceContentView {
            source_x: 0.0,
            source_y: 0.0,
            source_width: 2.0,
            source_height: 1.0,
            logical_width: 1.0,
            logical_height: 0.5,
        };
        let image = canonical_cursor(&[10, 20, 30, 127, 40, 50, 60, 255], 2, 1, view, (0.5, 0.0))
            .expect("canonical");
        assert_eq!(image.rgba(), &[30, 20, 10, 127, 60, 50, 40, 255]);
        assert_eq!(image.hotspot(), (1, 0));
        let view = SurfaceContentView {
            source_width: 256.0,
            source_height: 256.0,
            logical_width: 256.0,
            logical_height: 256.0,
            ..view
        };
        let image = canonical_cursor(&vec![255; 256 * 256 * 4], 256, 256, view, (128.0, 128.0))
            .expect("bounded");
        assert_eq!(
            (image.width(), image.height(), image.hotspot()),
            (128, 128, (64, 64))
        );
    }
}
