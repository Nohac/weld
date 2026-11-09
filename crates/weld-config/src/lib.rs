//! Shared source provenance and unsupported-capability diagnostics for configuration frontends.

use anyhow::{Context, Result};
use std::{error::Error, fmt};

/// Original source location retained through parsing and lowering.
#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    /// Source filename or diagnostic label.
    pub source: String,
    /// One-based physical line in the expanded source.
    pub line: usize,
    /// Byte range in the expanded source.
    pub range: std::ops::Range<usize>,
}
/// A typed syntax node with its source location.
#[derive(Clone, Debug, PartialEq)]
pub struct Located<T> {
    /// Typed node value.
    pub value: T,
    /// Source origin for diagnostics.
    pub location: Location,
}
/// Handling of recognized-but-unimplemented configuration features.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UnsupportedPolicy {
    /// Stop compilation on unsupported features.
    #[default]
    Reject,
    /// Skip unsupported features with a source-located warning.
    Warn,
}

/// A feature skipped during configuration compilation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigWarning {
    /// Source filename or caller-provided label.
    pub source: String,
    /// One-based physical line.
    pub line: usize,
    /// Explanation of the skipped feature.
    pub message: String,
}

/// Marks an unsupported feature, distinct from malformed supported settings.
#[derive(Debug)]
struct Unsupported(String);

impl fmt::Display for Unsupported {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl Error for Unsupported {}

/// Construct an error eligible for an explicit compatibility warning.
pub fn unsupported(message: impl Into<String>) -> anyhow::Error {
    Unsupported(message.into()).into()
}

impl UnsupportedPolicy {
    /// Retain unsupported-feature errors as warnings, or return the error.
    /// Malformed supported settings always remain errors.
    pub fn handle(
        self,
        error: anyhow::Error,
        source: &str,
        line: usize,
        warnings: &mut Vec<ConfigWarning>,
    ) -> Result<()> {
        if self == Self::Warn && error.downcast_ref::<Unsupported>().is_some() {
            warnings.push(ConfigWarning {
                source: source.into(),
                line,
                message: format!("{error:#}"),
            });
            Ok(())
        } else {
            Err(error).with_context(|| format!("{source}:{line}"))
        }
    }
}
