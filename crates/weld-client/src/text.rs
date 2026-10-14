//! Bounded, clipboard-independent text delivery through a keyboard route.

/// One UTF-8 typing request. Newlines and tabs become Enter and Tab keypresses.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(try_from = "String", into = "String"))]
#[derive(Clone, Eq, PartialEq)]
pub struct TextInput(String);

impl TextInput {
    pub const MAX_BYTES: usize = 4096;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TextInput {
    type Error = &'static str;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if text.is_empty() || text.len() > Self::MAX_BYTES {
            return Err("text must contain 1 to 4096 UTF-8 bytes");
        }
        if text
            .chars()
            .any(|character| character.is_control() && character != '\n' && character != '\t')
        {
            return Err("text contains a control character; only newline and tab are supported");
        }
        Ok(Self(text))
    }
}

impl From<TextInput> for String {
    fn from(text: TextInput) -> Self {
        text.0
    }
}

impl std::fmt::Debug for TextInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TextInput")
            .field("bytes", &self.0.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_bounds_validate_bytes_and_control_characters_without_exposing_content() {
        let text = TextInput::try_from("Hei æøå 世界 🦀\n\t".to_owned()).expect("Unicode text");
        assert!(!format!("{text:?}").contains("Hei"));
        assert!(TextInput::try_from("é".repeat(2048)).is_ok());
        for invalid in [
            String::new(),
            "é".repeat(2049),
            "a\0b".into(),
            "a\r\nb".into(),
            "\u{1b}".into(),
        ] {
            assert!(TextInput::try_from(invalid).is_err());
        }
    }
}
