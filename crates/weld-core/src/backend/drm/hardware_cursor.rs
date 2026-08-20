//! Kernel hardware-cursor presentation behind Weld's GPU fallback boundary.

use std::{collections::VecDeque, io};

use anyhow::{Context, Result, bail};
use smithay::{
    backend::{
        allocator::{
            Allocator, Fourcc, Modifier,
            gbm::{GbmAllocator, GbmBuffer, GbmBufferFlags, GbmDevice},
        },
        drm::DrmDeviceFd,
    },
    reexports::{
        drm::control::{Device as _, crtc},
        rustix::io::Errno,
    },
};
use tracing::{debug, info, warn};

use crate::renderer::{CursorPlaneImage, CursorPlaneSnapshot};

const RETIRED_CURSOR_BUFFERS: usize = 2;

struct CursorBuffer {
    image: CursorPlaneImage,
    buffer: GbmBuffer,
}

enum CursorFailure {
    Temporary(String),
    Permanent(String),
}

/// One output's optional kernel hardware-cursor state.
pub(super) struct HardwareCursor {
    drm: DrmDeviceFd,
    crtc: crtc::Handle,
    allocator: GbmAllocator<DrmDeviceFd>,
    extent: (u32, u32),
    desired: CursorPlaneSnapshot,
    applied: Option<CursorPlaneSnapshot>,
    current: Option<CursorBuffer>,
    current_hotspot: Option<(i32, i32)>,
    prepared: Option<CursorBuffer>,
    retired: VecDeque<CursorBuffer>,
    attached: bool,
    disabled: bool,
    activation_logged: bool,
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
            drm: drm.clone(),
            crtc,
            allocator: GbmAllocator::new(gbm, GbmBufferFlags::CURSOR | GbmBufferFlags::WRITE),
            extent,
            desired: CursorPlaneSnapshot::hidden(),
            applied: None,
            current: None,
            current_hotspot: None,
            prepared: None,
            retired: VecDeque::with_capacity(RETIRED_CURSOR_BUFFERS),
            attached: false,
            disabled: false,
            activation_logged: false,
        })
    }

    pub(super) const fn attached(&self) -> bool {
        self.attached
    }

    pub(super) fn set_desired(&mut self, desired: CursorPlaneSnapshot) {
        if self.desired == desired {
            return;
        }
        let same_prepared_image = self
            .prepared
            .as_ref()
            .zip(desired.image())
            .is_some_and(|(prepared, desired)| prepared.image == *desired);
        if !same_prepared_image {
            self.prepared = None;
        }
        self.desired = desired;
    }

    pub(super) fn apply(&mut self) {
        if self.applied.as_ref() == Some(&self.desired) {
            return;
        }
        if self.disabled || self.desired.image().is_none() {
            self.clear();
            return;
        }
        if let Err(error) = self.apply_visible() {
            match error {
                CursorFailure::Temporary(message) => {
                    debug!(%message, "hardware cursor update deferred");
                }
                CursorFailure::Permanent(message) => {
                    warn!(%message, "hardware cursor failed; using GPU cursor fallback");
                    self.disabled = true;
                    self.clear();
                }
            }
        }
    }

    pub(super) fn surface_was_cleared(&mut self) {
        self.attached = false;
        self.applied = None;
    }

    pub(super) fn suspend(&mut self) {
        self.clear();
        self.attached = false;
        self.applied = None;
    }

    fn apply_visible(&mut self) -> std::result::Result<(), CursorFailure> {
        let image =
            self.desired.image().cloned().ok_or_else(|| {
                CursorFailure::Permanent("visible cursor lost its image".to_owned())
            })?;
        let (width, height) = image.extent();
        if width > self.extent.0 || height > self.extent.1 {
            debug!(
                width,
                height,
                maximum_width = self.extent.0,
                maximum_height = self.extent.1,
                "cursor exceeds the hardware extent; using GPU cursor fallback"
            );
            self.clear();
            return Ok(());
        }

        let buffer_changed = requires_buffer_install(
            self.attached,
            self.current.as_ref().map(|current| &current.image),
            &image,
        );
        let cursor_changed = buffer_changed || self.current_hotspot != Some(self.desired.hotspot());
        if buffer_changed
            && !self
                .prepared
                .as_ref()
                .is_some_and(|prepared| prepared.image == image)
        {
            self.prepared = Some(
                self.prepare_buffer(image)
                    .map_err(|error| CursorFailure::Permanent(error.to_string()))?,
            );
        }
        if cursor_changed {
            let buffer = if buffer_changed {
                self.prepared.as_ref().map(|prepared| &prepared.buffer)
            } else {
                self.current.as_ref().map(|current| &current.buffer)
            }
            .ok_or_else(|| {
                CursorFailure::Permanent("cursor buffer disappeared before attachment".to_owned())
            })?;
            set_cursor(&self.drm, self.crtc, Some(buffer), self.desired.hotspot())
                .map_err(classify_cursor_error)?;
            if buffer_changed {
                let next = self.prepared.take().ok_or_else(|| {
                    CursorFailure::Permanent("attached cursor buffer disappeared".to_owned())
                })?;
                if let Some(previous) = self.current.replace(next) {
                    self.retired.push_back(previous);
                    while self.retired.len() > RETIRED_CURSOR_BUFFERS {
                        self.retired.pop_front();
                    }
                }
            }
            self.current_hotspot = Some(self.desired.hotspot());
            self.attached = true;
        }

        move_cursor(&self.drm, self.crtc, self.desired.origin()).map_err(classify_cursor_error)?;
        self.applied = Some(self.desired.clone());
        if !self.activation_logged {
            info!(?self.crtc, "hardware cursor is active");
            self.activation_logged = true;
        }
        Ok(())
    }

    fn clear(&mut self) {
        if !self.attached {
            return;
        }
        match set_cursor(&self.drm, self.crtc, None, (0, 0)) {
            Ok(()) => {
                self.attached = false;
                self.applied = None;
            }
            Err(error) => match classify_cursor_error(error) {
                CursorFailure::Temporary(message) => {
                    debug!(%message, "hardware cursor clear deferred");
                }
                CursorFailure::Permanent(message) => {
                    warn!(%message, "could not clear the hardware cursor");
                }
            },
        }
    }

    fn prepare_buffer(&mut self, image: CursorPlaneImage) -> Result<CursorBuffer> {
        let mut buffer = self
            .allocator
            .create_buffer(
                self.extent.0,
                self.extent.1,
                Fourcc::Argb8888,
                &[Modifier::Linear],
            )
            .context("failed to allocate a linear GBM cursor buffer")?;
        let pixels = rasterize_cursor(&image, self.extent)?;
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
        Ok(CursorBuffer { image, buffer })
    }
}

#[expect(
    deprecated,
    reason = "Weld's current presenter has no unified atomic primary-plus-cursor commit builder"
)]
fn set_cursor(
    drm: &DrmDeviceFd,
    crtc: crtc::Handle,
    buffer: Option<&GbmBuffer>,
    hotspot: (i32, i32),
) -> io::Result<()> {
    drm.set_cursor2(crtc, buffer, hotspot)
}

#[expect(
    deprecated,
    reason = "Weld's current presenter has no unified atomic primary-plus-cursor commit builder"
)]
fn move_cursor(drm: &DrmDeviceFd, crtc: crtc::Handle, position: (i32, i32)) -> io::Result<()> {
    drm.move_cursor(crtc, position)
}

fn classify_cursor_error(error: io::Error) -> CursorFailure {
    let message = error.to_string();
    if matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    ) || Errno::from_io_error(&error) == Some(Errno::BUSY)
    {
        CursorFailure::Temporary(message)
    } else {
        CursorFailure::Permanent(message)
    }
}

fn requires_buffer_install(
    attached: bool,
    current_image: Option<&CursorPlaneImage>,
    desired_image: &CursorPlaneImage,
) -> bool {
    !attached || current_image != Some(desired_image)
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

        assert!(!requires_buffer_install(true, Some(&image), &image));
        assert!(requires_buffer_install(false, Some(&image), &image));
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
