//! Lossless client cursor feedback, independent of surface commits and video.

use std::{fmt, sync::Arc};

use crate::ClientSurfaceId;

pub use cursor_icon::CursorIcon;

/// Maximum canonical bitmap extent. A complete RGBA image is at most 64 KiB.
pub const MAX_CURSOR_EXTENT: u16 = 128;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientCursor {
    Named(CursorIcon),
    Hidden,
    Image(ClientCursorImage),
}

impl Default for ClientCursor {
    fn default() -> Self {
        Self::Named(CursorIcon::Default)
    }
}

/// A canonical straight-alpha RGBA bitmap. Display size is destination policy.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(try_from = "WireCursorImage", into = "WireCursorImage")
)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientCursorImage {
    width: u16,
    height: u16,
    hotspot: (u16, u16),
    rgba: Arc<[u8]>,
}

impl ClientCursorImage {
    pub fn new(
        width: u16,
        height: u16,
        hotspot: (u16, u16),
        rgba: Vec<u8>,
    ) -> Result<Self, CursorImageError> {
        if width == 0 || height == 0 || width > MAX_CURSOR_EXTENT || height > MAX_CURSOR_EXTENT {
            return Err(CursorImageError(
                "cursor extent must be between 1 and 128 pixels",
            ));
        }
        if rgba.len() != usize::from(width) * usize::from(height) * 4 {
            return Err(CursorImageError(
                "cursor RGBA length does not match its extent",
            ));
        }
        if hotspot.0 >= width || hotspot.1 >= height {
            return Err(CursorImageError("cursor hotspot lies outside its image"));
        }
        Ok(Self {
            width,
            height,
            hotspot,
            rgba: rgba.into(),
        })
    }

    pub const fn width(&self) -> u16 {
        self.width
    }
    pub const fn height(&self) -> u16 {
        self.height
    }
    pub const fn hotspot(&self) -> (u16, u16) {
        self.hotspot
    }
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorImageError(&'static str);

impl fmt::Display for CursorImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for CursorImageError {}

#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
struct WireCursorImage {
    width: u16,
    height: u16,
    hotspot: (u16, u16),
    rgba: Vec<u8>,
}

#[cfg(feature = "serde")]
impl TryFrom<WireCursorImage> for ClientCursorImage {
    type Error = CursorImageError;
    fn try_from(raw: WireCursorImage) -> Result<Self, Self::Error> {
        Self::new(raw.width, raw.height, raw.hotspot, raw.rgba)
    }
}

#[cfg(feature = "serde")]
impl From<ClientCursorImage> for WireCursorImage {
    fn from(image: ClientCursorImage) -> Self {
        Self {
            width: image.width,
            height: image.height,
            hotspot: image.hotspot,
            rgba: image.rgba.to_vec(),
        }
    }
}

/// Shape feedback for a surface, not keyboard focus or permission to move a pointer.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientCursorUpdate {
    pub surface: ClientSurfaceId,
    pub cursor: ClientCursor,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_images_are_bounded_and_hotspots_are_inside_the_raster() {
        assert!(ClientCursorImage::new(0, 1, (0, 0), Vec::new()).is_err());
        assert!(ClientCursorImage::new(129, 1, (0, 0), vec![0; 516]).is_err());
        assert!(ClientCursorImage::new(1, 1, (0, 0), vec![0; 3]).is_err());
        assert!(ClientCursorImage::new(1, 1, (1, 0), vec![0; 4]).is_err());
        assert!(ClientCursorImage::new(128, 128, (127, 127), vec![0; 65536]).is_ok());
    }
}
