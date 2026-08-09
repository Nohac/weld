//! SHM buffer ownership, validation, and surface-view translation.

use anyhow::{Context, Result, anyhow, bail};
use smithay::{
    reexports::wayland_server::protocol::{wl_buffer, wl_output, wl_shm, wl_surface::WlSurface},
    utils::{Buffer as BufferCoord, Logical, Rectangle, Size, Transform},
    wayland::{
        compositor::with_states,
        shm::{BufferData, with_buffer_contents},
        viewporter::{ViewportCachedState, ensure_viewport_valid},
    },
};

use crate::surface::SurfaceContentView;

#[derive(Clone, Copy, Debug)]
pub(super) struct SurfaceBufferMetadata {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) scale: u32,
    pub(super) transform: wl_output::Transform,
}

pub(super) struct CopiedShmBuffer {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) bgra_pixels: Vec<u8>,
    pub(super) opaque: bool,
}

pub(super) fn copy_shm_buffer(buffer: &wl_buffer::WlBuffer) -> Result<CopiedShmBuffer> {
    if !cfg!(target_endian = "little") {
        bail!("the initial BGRA upload path requires a little-endian target");
    }

    with_buffer_contents(buffer, |pointer, pool_length, data| {
        copy_shm_contents(pointer, pool_length, data)
    })
    .context("buffer is not readable Wayland SHM")?
}

fn copy_shm_contents(
    pointer: *const u8,
    pool_length: usize,
    data: BufferData,
) -> Result<CopiedShmBuffer> {
    let width = usize::try_from(data.width).context("negative SHM width")?;
    let height = usize::try_from(data.height).context("negative SHM height")?;
    let stride = usize::try_from(data.stride).context("negative SHM stride")?;
    let offset = usize::try_from(data.offset).context("negative SHM offset")?;
    if width == 0 || height == 0 {
        bail!("zero-sized SHM buffer");
    }

    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| anyhow!("SHM row size overflow"))?;
    if stride < row_bytes {
        bail!("SHM stride is shorter than one pixel row");
    }
    let span = stride
        .checked_mul(height - 1)
        .and_then(|prefix| prefix.checked_add(row_bytes))
        .ok_or_else(|| anyhow!("SHM buffer size overflow"))?;
    let end = offset
        .checked_add(span)
        .ok_or_else(|| anyhow!("SHM pool offset overflow"))?;
    if end > pool_length {
        bail!("SHM buffer extends beyond its pool");
    }

    // SAFETY: Smithay guarantees the pointer is valid for pool_length bytes during this
    // callback. The client may not mutate an attached buffer before release; Weld copies it
    // synchronously and releases it only after this callback returns.
    let source = unsafe { std::slice::from_raw_parts(pointer.add(offset), span) };
    let pixels = normalize_bgra_rows(source, width, height, stride, data.format)?;

    Ok(CopiedShmBuffer {
        width: u32::try_from(width).context("SHM width exceeds u32")?,
        height: u32::try_from(height).context("SHM height exceeds u32")?,
        bgra_pixels: pixels,
        opaque: data.format == wl_shm::Format::Xrgb8888,
    })
}

pub(super) fn surface_content_view(
    surface: &WlSurface,
    metadata: SurfaceBufferMetadata,
) -> Result<SurfaceContentView> {
    if metadata.transform != wl_output::Transform::Normal {
        bail!(
            "unsupported client buffer transform {:?}; the initial SHM path supports only normal",
            metadata.transform
        );
    }
    let width = i32::try_from(metadata.width).context("client buffer width exceeds i32")?;
    let height = i32::try_from(metadata.height).context("client buffer height exceeds i32")?;
    let scale = i32::try_from(metadata.scale).context("client buffer scale exceeds i32")?;
    let logical_buffer_size =
        Size::<i32, BufferCoord>::from((width, height)).to_logical(scale, Transform::Normal);

    with_states(surface, |states| {
        if !ensure_viewport_valid(states, logical_buffer_size) {
            bail!("client viewport source extends outside its buffer");
        }
        let viewport = {
            let mut cached = states.cached_state.get::<ViewportCachedState>();
            *cached.current()
        };
        translate_surface_content_view(metadata, logical_buffer_size, viewport)
    })
}

fn translate_surface_content_view(
    metadata: SurfaceBufferMetadata,
    logical_buffer_size: Size<i32, Logical>,
    viewport: ViewportCachedState,
) -> Result<SurfaceContentView> {
    if metadata.transform != wl_output::Transform::Normal {
        bail!("only normal client buffer transforms can be translated");
    }
    let full_source = Rectangle::from_size(logical_buffer_size.to_f64());
    let source = viewport.src.unwrap_or(full_source);
    let destination = viewport.size().unwrap_or(logical_buffer_size);
    let source_right = source.loc.x + source.size.w;
    let source_bottom = source.loc.y + source.size.h;
    if !source.loc.x.is_finite()
        || !source.loc.y.is_finite()
        || !source.size.w.is_finite()
        || !source.size.h.is_finite()
        || source.loc.x < 0.0
        || source.loc.y < 0.0
        || source.size.w <= 0.0
        || source.size.h <= 0.0
        || source_right > f64::from(logical_buffer_size.w)
        || source_bottom > f64::from(logical_buffer_size.h)
        || destination.w <= 0
        || destination.h <= 0
    {
        bail!("invalid client surface viewport geometry");
    }

    let scale = f64::from(metadata.scale);
    let view = SurfaceContentView {
        source_x: (source.loc.x * scale) as f32,
        source_y: (source.loc.y * scale) as f32,
        source_width: (source.size.w * scale) as f32,
        source_height: (source.size.h * scale) as f32,
        logical_width: destination.w as f32,
        logical_height: destination.h as f32,
    };
    let values = [
        view.source_x,
        view.source_y,
        view.source_width,
        view.source_height,
        view.logical_width,
        view.logical_height,
    ];
    if !values.into_iter().all(f32::is_finite)
        || view.source_x + view.source_width > metadata.width as f32
        || view.source_y + view.source_height > metadata.height as f32
    {
        bail!("client surface viewport cannot be represented by the SHM image path");
    }
    Ok(view)
}

pub(super) fn checked_buffer_scale(buffer_scale: i32) -> Result<u32> {
    u32::try_from(buffer_scale)
        .ok()
        .filter(|scale| *scale > 0)
        .ok_or_else(|| anyhow!("invalid client buffer scale {buffer_scale}"))
}

fn normalize_bgra_rows(
    source: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    format: wl_shm::Format,
) -> Result<Vec<u8>> {
    if !matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888) {
        bail!("unsupported SHM format {format:?}");
    }
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| anyhow!("SHM row size overflow"))?;
    let required = stride
        .checked_mul(height.saturating_sub(1))
        .and_then(|prefix| prefix.checked_add(row_bytes))
        .ok_or_else(|| anyhow!("SHM source size overflow"))?;
    if stride < row_bytes || source.len() < required {
        bail!("invalid SHM row layout");
    }

    let mut pixels = Vec::with_capacity(
        row_bytes
            .checked_mul(height)
            .ok_or_else(|| anyhow!("SHM destination size overflow"))?,
    );
    for row in 0..height {
        let start = row * stride;
        pixels.extend_from_slice(&source[start..start + row_bytes]);
    }
    if format == wl_shm::Format::Xrgb8888 {
        for alpha in pixels[3..].iter_mut().step_by(4) {
            *alpha = u8::MAX;
        }
    }
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use smithay::{
        reexports::wayland_server::protocol::{wl_output, wl_shm},
        utils::{Logical, Rectangle, Size},
        wayland::viewporter::ViewportCachedState,
    };

    use super::*;

    fn metadata(width: u32, height: u32, scale: u32) -> SurfaceBufferMetadata {
        SurfaceBufferMetadata {
            width,
            height,
            scale,
            transform: wl_output::Transform::Normal,
        }
    }

    #[test]
    fn client_buffer_scale_must_be_positive() {
        assert_eq!(checked_buffer_scale(2).expect("valid scale"), 2);
        assert!(checked_buffer_scale(0).is_err());
        assert!(checked_buffer_scale(-1).is_err());
    }

    #[test]
    fn scale_only_surface_view_uses_the_full_buffer() {
        let view = translate_surface_content_view(
            metadata(1280, 960, 2),
            Size::<i32, Logical>::from((640, 480)),
            ViewportCachedState::default(),
        )
        .expect("valid scaled buffer");

        assert_eq!(
            view,
            SurfaceContentView {
                source_x: 0.0,
                source_y: 0.0,
                source_width: 1280.0,
                source_height: 960.0,
                logical_width: 640.0,
                logical_height: 480.0,
            }
        );
    }

    #[test]
    fn viewport_destination_defines_surface_logical_size() {
        let view = translate_surface_content_view(
            metadata(800, 600, 1),
            Size::<i32, Logical>::from((800, 600)),
            ViewportCachedState {
                src: None,
                dst: Some((640, 480).into()),
            },
        )
        .expect("valid fractional-scale viewport");

        assert_eq!(view.logical_width, 640.0);
        assert_eq!(view.logical_height, 480.0);
        assert_eq!(view.source_width, 800.0);
        assert_eq!(view.source_height, 600.0);
    }

    #[test]
    fn viewport_source_is_converted_from_logical_to_physical_pixels() {
        let view = translate_surface_content_view(
            metadata(1280, 960, 2),
            Size::<i32, Logical>::from((640, 480)),
            ViewportCachedState {
                src: Some(Rectangle::new((10.0, 20.0).into(), (100.0, 50.0).into())),
                dst: Some((200, 100).into()),
            },
        )
        .expect("valid cropped viewport");

        assert_eq!(view.source_x, 20.0);
        assert_eq!(view.source_y, 40.0);
        assert_eq!(view.source_width, 200.0);
        assert_eq!(view.source_height, 100.0);
        assert_eq!(view.logical_width, 200.0);
        assert_eq!(view.logical_height, 100.0);
    }

    #[test]
    fn rejects_out_of_bounds_viewport_sources() {
        let result = translate_surface_content_view(
            metadata(1280, 960, 2),
            Size::<i32, Logical>::from((640, 480)),
            ViewportCachedState {
                src: Some(Rectangle::new((600.0, 0.0).into(), (100.0, 50.0).into())),
                dst: Some((100, 50).into()),
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_non_normal_buffer_transforms() {
        let result = translate_surface_content_view(
            SurfaceBufferMetadata {
                transform: wl_output::Transform::_90,
                ..metadata(640, 480, 1)
            },
            Size::<i32, Logical>::from((480, 640)),
            ViewportCachedState::default(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn strips_row_padding() {
        let source = [
            1, 2, 3, 4, 5, 6, 7, 8, 99, 99, 9, 10, 11, 12, 13, 14, 15, 16, 88, 88,
        ];
        let pixels = normalize_bgra_rows(&source, 2, 2, 10, wl_shm::Format::Argb8888)
            .expect("valid padded pixels");
        assert_eq!(
            pixels,
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn preserves_argb_alpha() {
        let pixels = normalize_bgra_rows(&[3, 2, 1, 17], 1, 1, 4, wl_shm::Format::Argb8888)
            .expect("valid ARGB pixel");
        assert_eq!(pixels, [3, 2, 1, 17]);
    }

    #[test]
    fn forces_xrgb_alpha_opaque() {
        let pixels = normalize_bgra_rows(&[3, 2, 1, 0], 1, 1, 4, wl_shm::Format::Xrgb8888)
            .expect("valid XRGB pixel");
        assert_eq!(pixels, [3, 2, 1, 255]);
    }

    #[test]
    fn rejects_short_rows() {
        assert!(normalize_bgra_rows(&[0; 7], 2, 1, 7, wl_shm::Format::Argb8888).is_err());
    }
}
