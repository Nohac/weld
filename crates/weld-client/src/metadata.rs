//! Client-supplied display labels. These are never authenticated identities.
use std::fmt;

pub const MAX_SURFACE_LABEL_BYTES: usize = 1024;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(try_from = "WireMetadata", into = "WireMetadata")
)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClientSurfaceMetadata {
    app_id: String,
    title: String,
}

impl ClientSurfaceMetadata {
    pub fn new(app_id: String, title: String) -> Result<Self, SurfaceMetadataError> {
        if app_id.len() > MAX_SURFACE_LABEL_BYTES || title.len() > MAX_SURFACE_LABEL_BYTES {
            return Err(SurfaceMetadataError);
        }
        Ok(Self { app_id, title })
    }

    /// Bound native labels without rejecting an otherwise valid local client.
    /// Wire deserialization instead rejects oversized labels.
    pub fn truncated(mut app_id: String, mut title: String) -> Self {
        for text in [&mut app_id, &mut title] {
            let mut end = text.len().min(MAX_SURFACE_LABEL_BYTES);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        Self { app_id, title }
    }
    pub fn app_id(&self) -> &str {
        &self.app_id
    }
    pub fn title(&self) -> &str {
        &self.title
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceMetadataError;
impl fmt::Display for SurfaceMetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("surface label exceeds 1024 bytes")
    }
}
impl std::error::Error for SurfaceMetadataError {}

#[cfg(feature = "serde")]
#[derive(serde::Serialize, serde::Deserialize)]
struct WireMetadata {
    app_id: String,
    title: String,
}
#[cfg(feature = "serde")]
impl TryFrom<WireMetadata> for ClientSurfaceMetadata {
    type Error = SurfaceMetadataError;
    fn try_from(value: WireMetadata) -> Result<Self, Self::Error> {
        Self::new(value.app_id, value.title)
    }
}
#[cfg(feature = "serde")]
impl From<ClientSurfaceMetadata> for WireMetadata {
    fn from(value: ClientSurfaceMetadata) -> Self {
        Self {
            app_id: value.app_id,
            title: value.title,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_truncation_preserves_utf8_and_wire_bounds_are_strict() {
        let title = "é".repeat(513);
        assert!(ClientSurfaceMetadata::new(String::new(), title.clone()).is_err());
        let bounded = ClientSurfaceMetadata::truncated("x".repeat(1025), title);
        assert_eq!(bounded.app_id().len(), 1024);
        assert_eq!(bounded.title(), "é".repeat(512));
        assert!(ClientSurfaceMetadata::new("x".repeat(1024), String::new()).is_ok());
        #[cfg(feature = "serde")]
        assert!(
            ClientSurfaceMetadata::try_from(WireMetadata {
                app_id: String::new(),
                title: "x".repeat(1025)
            })
            .is_err()
        );
    }
}
