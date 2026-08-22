//! Backend-neutral cursor policy, theme discovery, and raster geometry.
//!
//! Smithay resources remain in the server boundary and presentation resources
//! remain in the native adapter. This module is the owned seam between them:
//! applications configure cursor policy, while core retains normalized client
//! pixels without exposing protocol objects.

use std::{env, num::NonZeroU32, sync::Arc};

use anyhow::{Result, bail};

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

#[derive(Clone, Debug)]
pub(crate) enum CursorImage {
    Hidden,
    Named(CursorIcon),
    Surface(ClientCursorImage),
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
