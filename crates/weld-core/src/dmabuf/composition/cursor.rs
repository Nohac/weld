//! Cursor composition shares the surface pipeline without inventing client buffer identities.

use super::{FORMAT, GeometrySlot, SourceCompositor, upload};
use crate::cursor::raster::capture_raster;
use anyhow::{Context, Result};
use weld_client::{ClientCursor, ComposedBuffer, Extent};

pub(super) struct CursorTexture {
    image: ClientCursor,
    scale: f64,
    width: u32,
    height: u32,
    hotspot: (u32, u32),
    binding: wgpu::BindGroup,
    geometry: GeometrySlot,
}

impl SourceCompositor {
    pub(super) fn prepare_cursor(&mut self, frame: &ComposedBuffer) -> Result<()> {
        self.cursor_visible = false;
        let Some(cursor) = &frame.cursor else {
            return Ok(());
        };
        if matches!(cursor.image, ClientCursor::Hidden) {
            return Ok(());
        }
        let scale = (f64::from(frame.extent.width) / f64::from(frame.logical_size.width))
            .max(f64::from(frame.extent.height) / f64::from(frame.logical_size.height));
        if self
            .cursor
            .as_ref()
            .is_none_or(|cached| cached.image != cursor.image || cached.scale != scale)
        {
            let Some(mut raster) =
                capture_raster(&cursor.image, &self.cursor_configuration, scale)?
            else {
                return Ok(());
            };
            for pixel in raster.rgba.as_chunks_mut::<4>().0 {
                let alpha = u32::from(pixel[3]);
                for channel in &mut pixel[..3] {
                    *channel = ((u32::from(*channel) * alpha + 127) / 255) as u8;
                }
                pixel.swap(0, 2);
            }
            let extent = Extent::new(raster.width, raster.height);
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("capture cursor"),
                size: wgpu::Extent3d {
                    width: extent.width,
                    height: extent.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            upload(&self.queue, &texture, &raster.rgba, extent)?;
            let view = texture.create_view(&Default::default());
            let binding = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("capture cursor pixels"),
                layout: &self.texture_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("capture cursor geometry"),
                size: 48,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let geometry_binding = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("capture cursor geometry"),
                layout: &self.geometry_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                }],
            });
            self.cursor = Some(CursorTexture {
                image: cursor.image.clone(),
                scale,
                width: raster.width,
                height: raster.height,
                hotspot: raster.hotspot,
                binding,
                geometry: GeometrySlot {
                    uploaded: None,
                    buffer,
                    binding: geometry_binding,
                },
            });
        }
        let cached = self
            .cursor
            .as_ref()
            .context("capture cursor raster absent")?;
        let scale = scale as f32;
        let values = [
            (cursor.position.x - cached.hotspot.0 as f32 / scale) / frame.logical_size.width,
            (cursor.position.y - cached.hotspot.1 as f32 / scale) / frame.logical_size.height,
            cached.width as f32 / scale / frame.logical_size.width,
            cached.height as f32 / scale / frame.logical_size.height,
            0.,
            0.,
            1.,
            1.,
            0.,
            0.,
            0.,
            0.,
        ];
        anyhow::ensure!(
            values.iter().all(|value| value.is_finite()),
            "invalid capture cursor geometry"
        );
        let mut bytes = [0; 48];
        for (value, destination) in values.iter().zip(bytes.as_chunks_mut::<4>().0) {
            *destination = value.to_ne_bytes();
        }
        self.queue.write_buffer(&cached.geometry.buffer, 0, &bytes);
        self.cursor_visible = true;
        Ok(())
    }

    pub(super) fn draw_cursor(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.cursor_visible
            && let Some(cursor) = &self.cursor
        {
            pass.set_bind_group(0, &cursor.binding, &[]);
            pass.set_bind_group(1, &cursor.geometry.binding, &[]);
            pass.draw(0..6, 0..1);
        }
    }
}
