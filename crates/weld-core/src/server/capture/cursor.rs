//! Cursor raster sampled alongside the output image, before asynchronous readback.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};

use crate::{OutputId, server::ServerState, surface::Extent};

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
        let Some(raster) = crate::cursor::raster::capture_raster(&image, configuration, scale)?
        else {
            return Ok(None);
        };
        let (pixels, width, height, hotspot) =
            (raster.rgba, raster.width, raster.height, raster.hotspot);
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
    pub(super) fn paint_gpu(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        blitter: &crate::renderer::CompositionBlitter,
        target: &wgpu::TextureView,
        encoder: &mut wgpu::CommandEncoder,
    ) -> Result<()> {
        let x = self.x.max(0).min(i64::from(target.texture().width()));
        let y = self.y.max(0).min(i64::from(target.texture().height()));
        let right = (self.x + i64::from(self.width)).min(i64::from(target.texture().width()));
        let bottom = (self.y + i64::from(self.height)).min(i64::from(target.texture().height()));
        if right <= x || bottom <= y {
            return Ok(());
        }
        let width = u32::try_from(right - x)?;
        let height = u32::try_from(bottom - y)?;
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for row in y..bottom {
            let start = usize::try_from(((row - self.y) * i64::from(self.width) + x - self.x) * 4)?;
            pixels.extend_from_slice(&self.pixels[start..start + width as usize * 4]);
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("output capture cursor"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            texture.as_image_copy(),
            &pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            texture.size(),
        );
        let binding = blitter.create_bind_group(
            device,
            "output capture cursor",
            &texture.create_view(&Default::default()),
        );
        blitter.encode_overlay(
            encoder,
            "capture cursor overlay",
            target,
            &binding,
            (x as f32, y as f32, width as f32, height as f32),
            [(x as u32, y as u32, width, height)],
        );
        Ok(())
    }

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
