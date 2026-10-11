//! Cursor raster sampled alongside the output image, before asynchronous readback.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};

use crate::{
    OutputId,
    cursor::{
        CursorImage,
        raster::{CropSource, destination_raster, resample_pixels, scaled_extent},
        theme::{fallback_image, load_theme_images},
    },
    server::ServerState,
    surface::Extent,
};

#[derive(Clone)]
pub(super) struct CaptureCursor {
    pixels: Arc<[u8]>,
    width: u32,
    height: u32,
    x: i64,
    y: i64,
}

impl ServerState {
    pub(super) fn capture_cursor(&self, id: OutputId) -> Result<Option<CaptureCursor>> {
        let output = self
            .outputs
            .get(&id)
            .context("capture output disappeared")?;
        let configuration = self.cursor_configuration();
        let scale = output.metrics.scale_factor();
        let position = self.local_input.pointer_position.get();
        let (x, y) = (
            ((position.x - f64::from(output.logical_position.0)) * scale).round() as i64,
            ((position.y - f64::from(output.logical_position.1)) * scale).round() as i64,
        );
        let image = self.presented_cursor.clone().unwrap_or_default();
        let (pixels, width, height, hotspot) = match image {
            CursorImage::Hidden => return Ok(None),
            CursorImage::Image(image) => {
                let raster = destination_raster(&image, configuration.size(), scale)?;
                (raster.rgba, raster.width, raster.height, raster.hotspot)
            }
            CursorImage::Named(icon) => {
                let images = load_theme_images(configuration.theme(), icon)
                    .unwrap_or_else(|_| Arc::from([fallback_image()]));
                let size = scaled_extent(f64::from(configuration.size()), scale)?;
                ensure!(size <= 512, "capture cursor exceeds raster bound");
                let image = images
                    .iter()
                    .min_by_key(|image| image.size.abs_diff(size))
                    .context("cursor theme contains no image")?;
                let pixels = resample_pixels(
                    &image.pixels_rgba,
                    image.width,
                    image.height,
                    size,
                    size,
                    CropSource::full(image.width, image.height),
                )?;
                let hotspot = (
                    (f64::from(image.xhot) / f64::from(image.width) * f64::from(size)).round()
                        as u32,
                    (f64::from(image.yhot) / f64::from(image.height) * f64::from(size)).round()
                        as u32,
                );
                (pixels, size, size, hotspot)
            }
        };
        Ok(Some(CaptureCursor {
            pixels: pixels.into(),
            width,
            height,
            x: x - i64::from(hotspot.0),
            y: y - i64::from(hotspot.1),
        }))
    }
}

impl CaptureCursor {
    pub(super) fn paint(&self, extent: Extent, pixels: &mut [u8]) -> Result<()> {
        let expected = u64::from(extent.width) * u64::from(extent.height) * 4;
        ensure!(
            u64::try_from(pixels.len())? == expected,
            "invalid cursor capture target"
        );
        for row in 0..self.height {
            let y = self.y + i64::from(row);
            if y < 0 || y >= i64::from(extent.height) {
                continue;
            }
            for column in 0..self.width {
                let x = self.x + i64::from(column);
                if x < 0 || x >= i64::from(extent.width) {
                    continue;
                }
                let source = usize::try_from(
                    (u64::from(row) * u64::from(self.width) + u64::from(column)) * 4,
                )?;
                let target = usize::try_from(
                    (u64::try_from(y)? * u64::from(extent.width) + u64::try_from(x)?) * 4,
                )?;
                let alpha = u32::from(self.pixels[source + 3]);
                for channel in 0..3 {
                    pixels[target + channel] = ((u32::from(self.pixels[source + channel]) * alpha
                        + u32::from(pixels[target + channel]) * (255 - alpha)
                        + 127)
                        / 255) as u8;
                }
                pixels[target + 3] = 255;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_cursor_clips_at_output_edges_and_blends_straight_alpha() {
        let cursor = CaptureCursor {
            pixels: Arc::from([255, 0, 0, 255, 200, 0, 0, 128]),
            width: 2,
            height: 1,
            x: -1,
            y: 1,
        };
        let mut pixels = [0, 100, 0, 255].repeat(4);
        cursor
            .paint(Extent::new(2, 2), &mut pixels)
            .expect("valid raster");
        assert_eq!(&pixels[8..12], &[100, 50, 0, 255]);
        assert_eq!(&pixels[..8], &[0, 100, 0, 255, 0, 100, 0, 255]);
        assert_eq!(&pixels[12..], &[0, 100, 0, 255]);
    }
}
