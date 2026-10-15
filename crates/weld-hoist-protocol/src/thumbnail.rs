//! On-demand application previews. Cache identities are scoped to one connection.

use serde::{Deserialize, Serialize};
use weld_client::ClientSurfaceId;

/// Initial preview size; producers preserve aspect ratio and avoid upscaling.
pub const MAX_EDGE: u32 = 320;
/// Bounds compressed storage and transfer independently of decoded dimensions.
pub const MAX_IMAGE_BYTES: usize = 512 * 1024;

/// Includes the occupant so a persistent slot cannot return its previous app's pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ThumbnailTarget {
    pub window: u64,
    pub surface: ClientSurfaceId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThumbnailRevision(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThumbnailRequest {
    pub target: ThumbnailTarget,
    pub if_revision: Option<ThumbnailRevision>,
}

/// An image header followed by exactly `png_bytes` raw bytes in the transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThumbnailImageHeader {
    pub width: u32,
    pub height: u32,
    pub png_bytes: u32,
}

impl ThumbnailImageHeader {
    /// Validate before allocating payload storage or configuring an image decoder.
    pub fn validate(self) -> Result<(), &'static str> {
        if self.width == 0 || self.height == 0 || self.width > MAX_EDGE || self.height > MAX_EDGE {
            return Err("thumbnail extent exceeds bounds");
        }
        if self.png_bytes == 0 || u64::from(self.png_bytes) > MAX_IMAGE_BYTES as u64 {
            return Err("thumbnail payload exceeds bounds");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThumbnailFreshness {
    /// Elapsed host time since capture completion, independent of endpoint clocks.
    pub age_ms: u64,
    /// The source changed since this snapshot; a refresh may be pending.
    pub stale: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThumbnailStatus {
    Image {
        revision: ThumbnailRevision,
        freshness: ThumbnailFreshness,
        image: ThumbnailImageHeader,
    },
    Unchanged {
        revision: ThumbnailRevision,
        freshness: ThumbnailFreshness,
    },
    Pending,
    Unavailable,
    Denied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThumbnailReply {
    pub target: ThumbnailTarget,
    pub status: ThumbnailStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_limits_bound_both_decoding_and_wire_allocation() {
        let valid = ThumbnailImageHeader {
            width: 320,
            height: 180,
            png_bytes: 1024,
        };
        assert!(valid.validate().is_ok());
        for invalid in [
            ThumbnailImageHeader { width: 0, ..valid },
            ThumbnailImageHeader {
                height: 321,
                ..valid
            },
            ThumbnailImageHeader {
                png_bytes: u32::MAX,
                ..valid
            },
            ThumbnailImageHeader {
                png_bytes: 0,
                ..valid
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }
}
