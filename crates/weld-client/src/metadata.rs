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
    x11_instance: Option<String>,
}

impl ClientSurfaceMetadata {
    pub fn new(app_id: String, title: String) -> Result<Self, SurfaceMetadataError> {
        if app_id.len() > MAX_SURFACE_LABEL_BYTES || title.len() > MAX_SURFACE_LABEL_BYTES {
            return Err(SurfaceMetadataError);
        }
        Ok(Self {
            app_id,
            title,
            x11_instance: None,
        })
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
        Self {
            app_id,
            title,
            x11_instance: None,
        }
    }
    /// X11 class remains the cross-platform application label; instance also
    /// identifies these labels as X11 properties for configuration criteria.
    pub fn truncated_x11(class: String, mut instance: String, title: String) -> Self {
        let mut metadata = Self::truncated(class, title);
        let mut end = instance.len().min(MAX_SURFACE_LABEL_BYTES);
        while !instance.is_char_boundary(end) {
            end -= 1;
        }
        instance.truncate(end);
        metadata.x11_instance = Some(instance);
        metadata
    }
    pub fn x11_class(&self) -> Option<&str> {
        self.x11_instance.as_ref().map(|_| self.app_id.as_str())
    }
    pub fn x11_instance(&self) -> Option<&str> {
        self.x11_instance.as_deref()
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
    x11_instance: Option<String>,
}
#[cfg(feature = "serde")]
impl TryFrom<WireMetadata> for ClientSurfaceMetadata {
    type Error = SurfaceMetadataError;
    fn try_from(value: WireMetadata) -> Result<Self, Self::Error> {
        let mut metadata = Self::new(value.app_id, value.title)?;
        if value
            .x11_instance
            .as_ref()
            .is_some_and(|value| value.len() > MAX_SURFACE_LABEL_BYTES)
        {
            return Err(SurfaceMetadataError);
        }
        metadata.x11_instance = value.x11_instance;
        Ok(metadata)
    }
}
#[cfg(feature = "serde")]
impl From<ClientSurfaceMetadata> for WireMetadata {
    fn from(value: ClientSurfaceMetadata) -> Self {
        Self {
            app_id: value.app_id,
            title: value.title,
            x11_instance: value.x11_instance,
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
                title: "x".repeat(1025),
                x11_instance: None,
            })
            .is_err()
        );
    }

    #[test]
    fn x11_labels_preserve_class_identity_and_bound_instance() {
        let native = ClientSurfaceMetadata::new("Demo".into(), String::new()).expect("native");
        assert_eq!(native.x11_class(), None);
        let x11 =
            ClientSurfaceMetadata::truncated_x11("Demo".into(), "é".repeat(513), String::new());
        assert_eq!(x11.x11_class(), Some("Demo"));
        assert_eq!(x11.x11_instance(), Some("é".repeat(512).as_str()));
        #[cfg(feature = "serde")]
        assert!(
            ClientSurfaceMetadata::try_from(WireMetadata {
                app_id: String::new(),
                title: String::new(),
                x11_instance: Some("x".repeat(1025))
            })
            .is_err()
        );
    }
}
