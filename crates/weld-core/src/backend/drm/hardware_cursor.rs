//! Cursor-plane buffer preparation behind Weld's GPU fallback boundary.

use anyhow::{Context, Result, bail};
use smithay::{
    backend::{
        allocator::{
            Allocator, Fourcc, Modifier,
            gbm::{GbmAllocator, GbmBuffer, GbmBufferFlags, GbmDevice},
        },
        drm::DrmDeviceFd,
    },
    reexports::drm::control::crtc,
};
use tracing::{debug, info, warn};

use crate::renderer::{CursorPlaneImage, CursorPlaneSnapshot};

#[derive(Debug)]
pub(super) enum PreparedCursorUpdate {
    Unchanged,
    Set {
        buffer: Option<GbmBuffer>,
        location: (i32, i32),
    },
    Move {
        location: (i32, i32),
    },
    FallbackExtent,
    Disabled,
}

/// Prepares GBM cursor buffers while Smithay owns their KMS lifetime.
pub(super) struct HardwareCursor {
    allocator: GbmAllocator<DrmDeviceFd>,
    extent: (u32, u32),
    applied: Option<CursorPlaneSnapshot>,
    prepared: Option<CursorPlaneSnapshot>,
    disabled: bool,
}

impl HardwareCursor {
    pub(super) fn new(drm: DrmDeviceFd, crtc: crtc::Handle, extent: (u32, u32)) -> Option<Self> {
        if extent.0 == 0 || extent.1 == 0 {
            info!("DRM device reports no hardware cursor extent; using GPU cursor fallback");
            return None;
        }
        let gbm = match GbmDevice::new(drm.clone()) {
            Ok(gbm) => gbm,
            Err(error) => {
                warn!(%error, "could not create the hardware cursor GBM device; using GPU cursor fallback");
                return None;
            }
        };
        debug!(
            ?crtc,
            width = extent.0,
            height = extent.1,
            "kernel hardware cursor resources are available"
        );
        Some(Self {
            allocator: GbmAllocator::new(gbm, GbmBufferFlags::CURSOR | GbmBufferFlags::WRITE),
            extent,
            applied: None,
            prepared: None,
            disabled: false,
        })
    }

    pub(super) fn disable(&mut self) {
        self.disabled = true;
        self.prepared = None;
    }

    pub(super) fn accept_prepared(&mut self) {
        if let Some(prepared) = self.prepared.take() {
            self.applied = Some(prepared);
        }
    }

    pub(super) fn reject_prepared(&mut self) {
        self.prepared = None;
    }

    pub(super) fn prepare(&mut self, desired: CursorPlaneSnapshot) -> PreparedCursorUpdate {
        if self.applied.as_ref() == Some(&desired) {
            self.prepared = None;
            return PreparedCursorUpdate::Unchanged;
        }
        if self.disabled {
            self.prepared = None;
            return PreparedCursorUpdate::Disabled;
        }

        let location = desired.origin();
        self.prepared = Some(desired.clone());
        let Some(image) = desired.image().cloned() else {
            return PreparedCursorUpdate::Set {
                buffer: None,
                location,
            };
        };
        let (width, height) = image.extent();
        if width > self.extent.0 || height > self.extent.1 {
            debug!(
                width,
                height,
                maximum_width = self.extent.0,
                maximum_height = self.extent.1,
                "cursor exceeds the hardware extent; using GPU cursor fallback"
            );
            return PreparedCursorUpdate::FallbackExtent;
        }

        let buffer_changed = requires_buffer_install(
            self.applied.as_ref().and_then(CursorPlaneSnapshot::image),
            &image,
        );
        if !buffer_changed {
            return PreparedCursorUpdate::Move { location };
        }

        match self.prepare_buffer(&image) {
            Ok(buffer) => PreparedCursorUpdate::Set {
                buffer: Some(buffer),
                location,
            },
            Err(error) => {
                warn!(%error, "hardware cursor buffer preparation failed; using GPU cursor fallback");
                self.disabled = true;
                self.prepared = None;
                PreparedCursorUpdate::Disabled
            }
        }
    }

    fn prepare_buffer(&mut self, image: &CursorPlaneImage) -> Result<GbmBuffer> {
        let mut buffer = self
            .allocator
            .create_buffer(
                self.extent.0,
                self.extent.1,
                Fourcc::Argb8888,
                &[Modifier::Linear],
            )
            .context("failed to allocate a linear GBM cursor buffer")?;
        let pixels = rasterize_cursor(image, self.extent)?;
        let width = self.extent.0;
        let height = self.extent.1;
        let source_stride = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .context("cursor row length overflowed")?;
        let rows = usize::try_from(height).context("cursor height overflowed")?;
        let write_result = buffer
            .map_mut(0, 0, width, height, |mapping| -> Result<()> {
                let destination_stride = usize::try_from(mapping.stride())
                    .context("cursor mapping stride overflowed")?;
                let required = rows
                    .checked_mul(destination_stride)
                    .context("cursor mapping length overflowed")?;
                if mapping.buffer_mut().len() < required || destination_stride < source_stride {
                    bail!("cursor mapping is smaller than its declared extent");
                }
                for row in 0..rows {
                    let source = &pixels[row * source_stride..(row + 1) * source_stride];
                    let destination = &mut mapping.buffer_mut()
                        [row * destination_stride..row * destination_stride + source_stride];
                    destination.copy_from_slice(source);
                }
                Ok(())
            })
            .context("failed to map the GBM cursor buffer")?;
        write_result?;
        Ok(buffer)
    }
}

fn requires_buffer_install(
    current_image: Option<&CursorPlaneImage>,
    desired_image: &CursorPlaneImage,
) -> bool {
    current_image != Some(desired_image)
}

fn rasterize_cursor(image: &CursorPlaneImage, plane_extent: (u32, u32)) -> Result<Vec<u8>> {
    let (texture_width, texture_height) = image.texture_extent();
    let (source_x, source_y, source_width, source_height) = image.source();
    let (width, height) = image.extent();
    if texture_width == 0
        || texture_height == 0
        || width == 0
        || height == 0
        || width > plane_extent.0
        || height > plane_extent.1
        || !source_x.is_finite()
        || !source_y.is_finite()
        || !source_width.is_finite()
        || !source_height.is_finite()
        || source_width <= 0.0
        || source_height <= 0.0
    {
        bail!("invalid cursor raster geometry");
    }
    let texture_bytes = usize::try_from(texture_width)
        .ok()
        .and_then(|width| {
            usize::try_from(texture_height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .context("cursor texture extent overflowed")?;
    if image.pixels().len() != texture_bytes {
        bail!("cursor texture byte length does not match its extent");
    }
    let plane_bytes = usize::try_from(plane_extent.0)
        .ok()
        .and_then(|width| {
            usize::try_from(plane_extent.1)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .context("cursor plane extent overflowed")?;
    let mut output = vec![0_u8; plane_bytes];
    let destination_stride =
        usize::try_from(plane_extent.0).context("cursor plane width overflowed")? * 4;
    for y in 0..height {
        for x in 0..width {
            let sample_x = source_x + (x as f32 + 0.5) * source_width / width as f32 - 0.5;
            let sample_y = source_y + (y as f32 + 0.5) * source_height / height as f32 - 0.5;
            let pixel = bilinear_premultiplied_bgra(
                image.pixels(),
                texture_width,
                texture_height,
                sample_x,
                sample_y,
            );
            let offset = y as usize * destination_stride + x as usize * 4;
            output[offset..offset + 4].copy_from_slice(&pixel);
        }
    }
    Ok(output)
}

fn bilinear_premultiplied_bgra(pixels: &[u8], width: u32, height: u32, x: f32, y: f32) -> [u8; 4] {
    let x = x.clamp(0.0, width.saturating_sub(1) as f32);
    let y = y.clamp(0.0, height.saturating_sub(1) as f32);
    let left = x.floor() as u32;
    let top = y.floor() as u32;
    let right = left.saturating_add(1).min(width.saturating_sub(1));
    let bottom = top.saturating_add(1).min(height.saturating_sub(1));
    let horizontal = x - left as f32;
    let vertical = y - top as f32;
    let sample = |sample_x: u32, sample_y: u32, channel: usize| {
        let offset = ((sample_y * width + sample_x) * 4) as usize;
        let value = pixels[offset + channel] as f32;
        if channel == 3 {
            value
        } else {
            value * pixels[offset + 3] as f32 / 255.0
        }
    };
    let mut mixed = [0.0_f32; 4];
    for (channel, destination) in mixed.iter_mut().enumerate() {
        let upper = sample(left, top, channel) * (1.0 - horizontal)
            + sample(right, top, channel) * horizontal;
        let lower = sample(left, bottom, channel) * (1.0 - horizontal)
            + sample(right, bottom, channel) * horizontal;
        *destination = upper * (1.0 - vertical) + lower * vertical;
    }
    let alpha = mixed[3].clamp(0.0, 255.0) as u8;
    let mut output = [0_u8; 4];
    for channel in 0..3 {
        output[channel] = (mixed[channel].clamp(0.0, 255.0) as u8).min(alpha);
    }
    output[3] = alpha;
    output
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{cursor::CursorGeometry, renderer::CursorPlaneImage};

    use super::{rasterize_cursor, requires_buffer_install};

    fn image(
        pixels: &[u8],
        texture_extent: (u32, u32),
        source: (f32, f32, f32, f32),
        extent: (u32, u32),
    ) -> CursorPlaneImage {
        CursorPlaneImage::new(
            Arc::from(pixels),
            texture_extent.0,
            texture_extent.1,
            CursorGeometry {
                origin_x: 0.0,
                origin_y: 0.0,
                width: extent.0 as f32,
                height: extent.1 as f32,
                source_x: source.0,
                source_y: source.1,
                source_width: source.2,
                source_height: source.3,
            },
            extent.0,
            extent.1,
        )
    }

    #[test]
    fn rasterization_scales_to_natural_size_and_pads_the_plane() {
        let image = image(&[200, 100, 50, 128], (1, 1), (0.0, 0.0, 1.0, 1.0), (2, 1));
        let raster = rasterize_cursor(&image, (4, 2)).expect("valid cursor raster");

        assert_eq!(&raster[..8], &[100, 50, 25, 128, 100, 50, 25, 128]);
        assert!(raster[8..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn rasterization_respects_the_client_surface_crop() {
        let image = image(
            &[1, 2, 3, 255, 4, 5, 6, 255],
            (2, 1),
            (1.0, 0.0, 1.0, 1.0),
            (1, 1),
        );
        let raster = rasterize_cursor(&image, (2, 1)).expect("valid cropped cursor raster");

        assert_eq!(&raster[..4], &[4, 5, 6, 255]);
        assert_eq!(&raster[4..], &[0, 0, 0, 0]);
    }

    #[test]
    fn position_only_updates_reuse_the_installed_cursor_buffer() {
        let image = image(&[0, 0, 0, 255], (1, 1), (0.0, 0.0, 1.0, 1.0), (1, 1));

        assert!(!requires_buffer_install(Some(&image), &image));
        assert!(requires_buffer_install(None, &image));
    }

    #[test]
    fn resampling_interpolates_premultiplied_edge_texels() {
        let image = image(
            &[0, 0, 0, 0, 255, 255, 255, 255],
            (2, 1),
            (0.0, 0.0, 2.0, 1.0),
            (1, 1),
        );
        let raster = rasterize_cursor(&image, (1, 1)).expect("valid translucent cursor edge");

        assert_eq!(raster, [127, 127, 127, 127]);
    }
}
