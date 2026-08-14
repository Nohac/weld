//! Backend-neutral cursor policy, theme discovery, and raster geometry.
//!
//! Smithay resources remain in the server boundary and GPU resources remain in
//! the renderer. This module is the owned seam between them: applications can
//! configure a logical cursor size, while the host resolves named Xcursor
//! images or normalized client-provided pixels without exposing either API.

use std::{collections::HashMap, env, fs, num::NonZeroU32, sync::Arc, time::Duration};

use anyhow::{Result, bail};
use tracing::warn;
use xcursor::{CursorTheme, parser::parse_xcursor};

use crate::surface::SurfaceContentView;

pub use smithay::input::pointer::CursorIcon;

const DEFAULT_CURSOR_SIZE: u32 = 24;
const DEFAULT_CURSOR_THEME: &str = "default";

/// Reloadable cursor theme and logical nominal size.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorConfiguration {
    theme: String,
    size: NonZeroU32,
}

impl CursorConfiguration {
    /// Creates validated cursor configuration.
    pub fn new(theme: impl Into<String>, size: u32) -> Result<Self> {
        let Some(size) = NonZeroU32::new(size) else {
            bail!("cursor size must be positive");
        };
        let theme = theme.into();
        let theme = if theme.trim().is_empty() {
            DEFAULT_CURSOR_THEME.to_owned()
        } else {
            theme
        };
        Ok(Self { theme, size })
    }

    /// Uses the conventional Xcursor environment with durable defaults.
    pub fn from_environment() -> Self {
        let theme = env::var("XCURSOR_THEME")
            .ok()
            .filter(|theme| !theme.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_CURSOR_THEME.to_owned());
        let size = env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|size| size.parse::<u32>().ok())
            .and_then(NonZeroU32::new)
            .unwrap_or_else(|| NonZeroU32::new(DEFAULT_CURSOR_SIZE).unwrap_or(NonZeroU32::MIN));
        Self { theme, size }
    }

    /// Returns the Xcursor theme name.
    pub fn theme(&self) -> &str {
        &self.theme
    }

    /// Returns the nominal cursor size in logical pixels.
    pub const fn size(&self) -> u32 {
        self.size.get()
    }
}

impl Default for CursorConfiguration {
    fn default() -> Self {
        Self::from_environment()
    }
}

/// Cursor requested by Weld-owned UI while the pointer is outside a client surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorAppearance {
    Hidden,
    Named(CursorIcon),
}

impl Default for CursorAppearance {
    fn default() -> Self {
        Self::Named(CursorIcon::Default)
    }
}

/// Changes published by the application layer to the native cursor host.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CursorHostUpdate {
    pub configuration: Option<CursorConfiguration>,
    pub appearance: Option<CursorAppearance>,
}

impl CursorHostUpdate {
    pub const fn is_empty(&self) -> bool {
        self.configuration.is_none() && self.appearance.is_none()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ClientCursorImage {
    pub(crate) pixels: Arc<[u8]>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) view: SurfaceContentView,
    pub(crate) hotspot_x: f32,
    pub(crate) hotspot_y: f32,
}

impl ClientCursorImage {
    pub(crate) fn same_image(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.pixels, &other.pixels)
            && self.width == other.width
            && self.height == other.height
            && self.view == other.view
            && self.hotspot_x == other.hotspot_x
            && self.hotspot_y == other.hotspot_y
    }
}

#[derive(Clone, Debug)]
pub(crate) enum CursorImage {
    Hidden,
    Named(CursorIcon),
    Surface(ClientCursorImage),
}

impl CursorImage {
    pub(crate) fn same_image(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Hidden, Self::Hidden) => true,
            (Self::Named(left), Self::Named(right)) => left == right,
            (Self::Surface(left), Self::Surface(right)) => left.same_image(right),
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ThemeCursorFrame {
    pub(crate) pixels: Arc<[u8]>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) nominal_size: u32,
    pub(crate) hotspot_x: u32,
    pub(crate) hotspot_y: u32,
    delay_milliseconds: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CursorGeometry {
    pub(crate) origin_x: f32,
    pub(crate) origin_y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) source_x: f32,
    pub(crate) source_y: f32,
    pub(crate) source_width: f32,
    pub(crate) source_height: f32,
}

pub(crate) struct SelectedThemeFrame {
    pub(crate) frame: Arc<ThemeCursorFrame>,
    pub(crate) next_frame_after: Option<Duration>,
}

pub(crate) struct XcursorResolver {
    theme: CursorTheme,
    frames: HashMap<CursorIcon, Arc<[Arc<ThemeCursorFrame>]>>,
}

impl XcursorResolver {
    pub(crate) fn new(configuration: &CursorConfiguration) -> Self {
        Self {
            theme: CursorTheme::load(configuration.theme()),
            frames: HashMap::new(),
        }
    }

    pub(crate) fn frame(
        &mut self,
        icon: CursorIcon,
        physical_nominal_size: u32,
        elapsed: Duration,
    ) -> SelectedThemeFrame {
        let frames = self
            .frames
            .entry(icon)
            .or_insert_with(|| load_cursor_frames(&self.theme, icon));
        select_theme_frame(frames, physical_nominal_size, elapsed)
    }
}

pub(crate) fn named_cursor_geometry(
    pointer_x: f64,
    pointer_y: f64,
    output_scale: f64,
    logical_nominal_size: u32,
    frame: &ThemeCursorFrame,
) -> CursorGeometry {
    let nominal_size = frame.nominal_size.max(1) as f64;
    let scale = output_scale * f64::from(logical_nominal_size) / nominal_size;
    CursorGeometry {
        origin_x: (pointer_x * output_scale - f64::from(frame.hotspot_x) * scale) as f32,
        origin_y: (pointer_y * output_scale - f64::from(frame.hotspot_y) * scale) as f32,
        width: (f64::from(frame.width) * scale) as f32,
        height: (f64::from(frame.height) * scale) as f32,
        source_x: 0.0,
        source_y: 0.0,
        source_width: frame.width as f32,
        source_height: frame.height as f32,
    }
}

pub(crate) fn client_cursor_geometry(
    pointer_x: f64,
    pointer_y: f64,
    output_scale: f64,
    logical_nominal_size: u32,
    image: &ClientCursorImage,
) -> CursorGeometry {
    let logical_width = image.view.logical_width.max(f32::EPSILON);
    let logical_height = image.view.logical_height.max(f32::EPSILON);
    let normalization = logical_nominal_size as f32 / logical_width.max(logical_height);
    let physical_scale = normalization * output_scale as f32;
    CursorGeometry {
        origin_x: (pointer_x * output_scale) as f32 - image.hotspot_x * physical_scale,
        origin_y: (pointer_y * output_scale) as f32 - image.hotspot_y * physical_scale,
        width: logical_width * physical_scale,
        height: logical_height * physical_scale,
        source_x: image.view.source_x,
        source_y: image.view.source_y,
        source_width: image.view.source_width,
        source_height: image.view.source_height,
    }
}

pub(crate) fn unpremultiply_bgra(pixels: &mut [u8]) {
    unpremultiply_channels(pixels, [0, 1, 2]);
}

fn unpremultiply_channels(pixels: &mut [u8], channels: [usize; 3]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 {
            for channel in channels {
                pixel[channel] = 0;
            }
            continue;
        }
        for channel in channels {
            let straight = (u32::from(pixel[channel]) * 255 + alpha / 2) / alpha;
            pixel[channel] = straight.min(255) as u8;
        }
    }
}

fn load_cursor_frames(theme: &CursorTheme, requested: CursorIcon) -> Arc<[Arc<ThemeCursorFrame>]> {
    let mut names = Vec::with_capacity(2 + requested.alt_names().len());
    names.push(requested.name());
    names.extend_from_slice(requested.alt_names());
    if requested != CursorIcon::Default {
        names.push(CursorIcon::Default.name());
        names.extend_from_slice(CursorIcon::Default.alt_names());
    }

    for name in names {
        let Some(path) = theme.load_icon(name) else {
            continue;
        };
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(error) => {
                warn!(path = %path.display(), %error, "could not read Xcursor image");
                continue;
            }
        };
        let Some(images) = parse_xcursor(&data) else {
            warn!(path = %path.display(), "could not parse Xcursor image");
            continue;
        };
        let frames = images
            .into_iter()
            .map(|image| {
                // Xcursor stores native ARGB words. On Weld's little-endian target the
                // parser's raw `pixels_rgba` bytes are therefore ordered BGRA.
                let mut pixels = image.pixels_rgba;
                unpremultiply_bgra(&mut pixels);
                Arc::new(ThemeCursorFrame {
                    pixels: pixels.into(),
                    width: image.width,
                    height: image.height,
                    nominal_size: image.size.max(1),
                    hotspot_x: image.xhot,
                    hotspot_y: image.yhot,
                    delay_milliseconds: image.delay,
                })
            })
            .collect::<Vec<_>>();
        if !frames.is_empty() {
            return frames.into();
        }
    }

    warn!(
        shape = requested.name(),
        "Xcursor shape is unavailable; using Weld fallback"
    );
    Arc::from([Arc::new(fallback_arrow())])
}

fn select_theme_frame(
    frames: &[Arc<ThemeCursorFrame>],
    physical_nominal_size: u32,
    elapsed: Duration,
) -> SelectedThemeFrame {
    let nearest_nominal = frames
        .iter()
        .min_by_key(|frame| frame.nominal_size.abs_diff(physical_nominal_size))
        .map_or(DEFAULT_CURSOR_SIZE, |frame| frame.nominal_size);
    let candidates = frames
        .iter()
        .filter(|frame| frame.nominal_size == nearest_nominal)
        .collect::<Vec<_>>();
    if candidates.len() <= 1 {
        return SelectedThemeFrame {
            frame: candidates
                .first()
                .map_or_else(|| Arc::new(fallback_arrow()), |frame| Arc::clone(frame)),
            next_frame_after: None,
        };
    }

    let total = candidates
        .iter()
        .map(|frame| u64::from(frame.delay_milliseconds))
        .sum::<u64>();
    if total == 0 {
        return SelectedThemeFrame {
            frame: Arc::clone(candidates[0]),
            next_frame_after: None,
        };
    }

    let mut offset = (elapsed.as_millis() as u64) % total;
    for frame in candidates {
        let delay = u64::from(frame.delay_milliseconds);
        if delay > 0 && offset < delay {
            return SelectedThemeFrame {
                frame: Arc::clone(frame),
                next_frame_after: Some(Duration::from_millis((delay - offset).max(1))),
            };
        }
        offset = offset.saturating_sub(delay);
    }

    let frame = frames
        .first()
        .map_or_else(|| Arc::new(fallback_arrow()), Arc::clone);
    SelectedThemeFrame {
        frame,
        next_frame_after: None,
    }
}

fn fallback_arrow() -> ThemeCursorFrame {
    let width = DEFAULT_CURSOR_SIZE;
    let height = DEFAULT_CURSOR_SIZE;
    let mut pixels = vec![0_u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let head = y < 17 && x <= y / 2;
            let stem = (5..=8).contains(&x) && (11..=22).contains(&y);
            if !head && !stem {
                continue;
            }
            let border = x == 0
                || y == 0
                || (head && (x == y / 2 || y == 16))
                || (stem && (x == 5 || x == 8 || y == 22));
            let offset = ((y * width + x) * 4) as usize;
            let color = if border { 24 } else { 245 };
            pixels[offset..offset + 4].copy_from_slice(&[color, color, color, 255]);
        }
    }
    ThemeCursorFrame {
        pixels: pixels.into(),
        width,
        height,
        nominal_size: DEFAULT_CURSOR_SIZE,
        hotspot_x: 1,
        hotspot_y: 1,
        delay_milliseconds: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(nominal_size: u32, delay_milliseconds: u32, marker: u8) -> Arc<ThemeCursorFrame> {
        Arc::new(ThemeCursorFrame {
            pixels: Arc::from([marker, marker, marker, 255]),
            width: nominal_size,
            height: nominal_size,
            nominal_size,
            hotspot_x: 2,
            hotspot_y: 3,
            delay_milliseconds,
        })
    }

    #[test]
    fn cursor_animation_selects_the_nearest_nominal_group_and_next_deadline() {
        let frames = [frame(24, 10, 1), frame(24, 20, 2), frame(48, 0, 3)];
        let selected = select_theme_frame(&frames, 25, Duration::from_millis(15));
        assert_eq!(selected.frame.pixels[0], 2);
        assert_eq!(selected.next_frame_after, Some(Duration::from_millis(15)));

        let large = select_theme_frame(&frames, 48, Duration::from_secs(4));
        assert_eq!(large.frame.pixels[0], 3);
        assert_eq!(large.next_frame_after, None);
    }

    #[test]
    fn cursor_geometry_applies_one_scale_to_extent_and_hotspot() {
        let frame = ThemeCursorFrame {
            pixels: Arc::from([]),
            width: 32,
            height: 40,
            nominal_size: 16,
            hotspot_x: 4,
            hotspot_y: 6,
            delay_milliseconds: 0,
        };
        let geometry = named_cursor_geometry(80.0, 40.0, 1.25, 24, &frame);
        assert_eq!(geometry.origin_x, 92.5);
        assert_eq!(geometry.origin_y, 38.75);
        assert_eq!(geometry.width, 60.0);
        assert_eq!(geometry.height, 75.0);
    }

    #[test]
    fn client_cursor_size_is_normalized_independently_of_client_dimensions() {
        let image = ClientCursorImage {
            pixels: Arc::from([]),
            width: 64,
            height: 32,
            view: SurfaceContentView {
                source_x: 0.0,
                source_y: 0.0,
                source_width: 64.0,
                source_height: 32.0,
                logical_width: 64.0,
                logical_height: 32.0,
            },
            hotspot_x: 8.0,
            hotspot_y: 4.0,
        };
        let geometry = client_cursor_geometry(100.0, 50.0, 2.0, 24, &image);
        assert_eq!(geometry.width, 48.0);
        assert_eq!(geometry.height, 24.0);
        assert_eq!(geometry.origin_x, 194.0);
        assert_eq!(geometry.origin_y, 97.0);
    }
}
