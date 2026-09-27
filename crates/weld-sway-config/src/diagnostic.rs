//! Structural diagnostics, independent of Winnow's error representation.

use std::{error::Error, fmt, ops::Range};

use crate::MAX_BLOCK_DEPTH;

/// Structural failures recognized before any command semantics are applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticKind {
    /// A standalone opening brace has no eligible preceding header.
    UnexpectedOpeningBrace,
    /// A closing brace has no corresponding open block.
    UnexpectedClosingBrace,
    /// Weld refuses to discard command text preceding a closing brace.
    ContentBeforeClosingBrace,
    /// An opening brace was not closed before the source ended.
    UnclosedBlock,
    /// Another block would exceed [`crate::MAX_BLOCK_DEPTH`].
    NestingLimit,
    /// An argument could not be scanned at the reported position.
    InvalidArgument,
}

impl fmt::Display for DiagnosticKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedOpeningBrace => {
                formatter.write_str("opening brace has no preceding statement")
            }
            Self::UnexpectedClosingBrace => formatter.write_str("closing brace has no open block"),
            Self::ContentBeforeClosingBrace => formatter.write_str(
                "closing brace must stand alone; Weld does not discard preceding command text",
            ),
            Self::UnclosedBlock => formatter.write_str("block is not closed before end of source"),
            Self::NestingLimit => write!(
                formatter,
                "block nesting exceeds Weld's limit of {MAX_BLOCK_DEPTH}"
            ),
            Self::InvalidArgument => formatter.write_str("could not parse argument"),
        }
    }
}

/// The first structural failure, located in the original physical source.
///
/// Lines and columns are one-based. Columns count Unicode scalar values, not
/// bytes, graphemes or terminal cells. The byte span is always authoritative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    source_name: String,
    kind: DiagnosticKind,
    span: Range<usize>,
    line: usize,
    column: usize,
    excerpt: String,
    marker_padding: usize,
    marker_width: usize,
}

impl Diagnostic {
    /// Structural reason for failure.
    pub fn kind(&self) -> DiagnosticKind {
        self.kind
    }

    /// Caller-supplied source label; never opened as a file.
    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    /// Original half-open UTF-8 byte span.
    pub fn span(&self) -> Range<usize> {
        self.span.clone()
    }

    /// One-based physical source line.
    pub fn line(&self) -> usize {
        self.line
    }

    /// One-based Unicode-scalar column within the physical line.
    pub fn column(&self) -> usize {
        self.column
    }

    pub(crate) fn new(
        source_name: &str,
        source: &str,
        span: Range<usize>,
        kind: DiagnosticKind,
    ) -> Self {
        let prefix = &source[..span.start];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let line_start = prefix.rfind('\n').map_or(0, |offset| offset + 1);
        let line_end = source[span.start..]
            .find('\n')
            .map_or(source.len(), |offset| span.start + offset);
        let line_prefix = &source[line_start..span.start];
        Self {
            source_name: source_name.to_owned(),
            kind,
            span: span.clone(),
            line,
            column: line_prefix.chars().count() + 1,
            excerpt: visible(source[line_start..line_end].trim_end_matches('\r')),
            marker_padding: visible(line_prefix).chars().count(),
            marker_width: visible(&source[span.start..span.end.min(line_end)])
                .chars()
                .count()
                .max(1),
        }
    }
}

// Keep control characters in source excerpts from becoming terminal commands.
fn visible(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\t' {
            output.push_str("    ");
        } else if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "{}:{}:{}: {}",
            visible(&self.source_name),
            self.line,
            self.column,
            self.kind
        )?;
        writeln!(formatter, "{} | {}", self.line, self.excerpt)?;
        write!(
            formatter,
            "{} | {}{}",
            " ".repeat(self.line.to_string().len()),
            " ".repeat(self.marker_padding),
            "^".repeat(self.marker_width)
        )
    }
}

impl Error for Diagnostic {}
