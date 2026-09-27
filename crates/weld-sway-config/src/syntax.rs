//! Owned spelling with physical-source provenance. Only the parser constructs it.

use std::ops::Range;

/// Parsed statements from one source, before expansion or command validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedConfig {
    pub(crate) statements: Vec<Statement>,
}

impl ParsedConfig {
    /// Top-level statements in source order.
    pub fn statements(&self) -> &[Statement] {
        &self.statements
    }
}

/// A directive's unexpanded spelling and optional nested config block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    pub(crate) header: RawText,
    pub(crate) name: RawText,
    pub(crate) arguments: Vec<RawText>,
    pub(crate) block: Option<Block>,
}

impl Statement {
    /// Complete trimmed header, preserving internal whitespace and excluding `{`.
    pub fn header(&self) -> &RawText {
        &self.header
    }

    /// Unexpanded first argument, which may itself be a variable.
    pub fn name(&self) -> &RawText {
        &self.name
    }

    /// Arguments after the name, with quoting, escapes and punctuation intact.
    pub fn arguments(&self) -> &[RawText] {
        &self.arguments
    }

    /// Nested statements, if the header opens a block.
    pub fn block(&self) -> Option<&Block> {
        self.block.as_ref()
    }
}

/// A multiline block, without prefix expansion or command-specific meaning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub(crate) opening: Range<usize>,
    pub(crate) closing: Range<usize>,
    pub(crate) statements: Vec<Statement>,
}

impl Block {
    /// Original half-open byte range of the opening brace.
    pub fn opening(&self) -> Range<usize> {
        self.opening.clone()
    }

    /// Original half-open byte range of the closing brace.
    pub fn closing(&self) -> Range<usize> {
        self.closing.clone()
    }

    /// Children in source order; parent prefixes have not been applied.
    pub fn statements(&self) -> &[Statement] {
        &self.statements
    }
}

/// Raw spelling assembled from one or more original source segments.
///
/// Quotes, escapes and variables remain as written. Only backslash-LF
/// continuations are removed. Segments are ordered half-open UTF-8 byte ranges;
/// joining their source slices exactly reconstructs [`Self::text`]. For headers,
/// they cover the trimmed extent, not all physical lines or surrounding comments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawText {
    text: String,
    segments: Vec<Range<usize>>,
    logical_starts: Vec<usize>,
}

impl RawText {
    /// Spelling after continuation removal, without unquoting or expansion.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Original source pieces making up this spelling.
    pub fn segments(&self) -> &[Range<usize>] {
        &self.segments
    }

    pub(crate) fn new(source: &str, segments: Vec<Range<usize>>) -> Self {
        let mut text = String::new();
        let mut logical_starts = Vec::with_capacity(segments.len());
        for segment in &segments {
            logical_starts.push(text.len());
            // Only source-derived UTF-8 boundaries enter this private constructor.
            text.push_str(&source[segment.clone()]);
        }
        Self {
            text,
            segments,
            logical_starts,
        }
    }

    /// Maps a parser-produced logical range back across continuation gaps.
    pub(crate) fn slice(&self, source: &str, range: Range<usize>) -> Self {
        let first = self
            .logical_starts
            .partition_point(|start| *start <= range.start)
            .saturating_sub(1);
        let mut segments = Vec::new();
        for (segment, start) in self.segments[first..]
            .iter()
            .zip(&self.logical_starts[first..])
        {
            if *start >= range.end {
                break;
            }
            let left = range.start.max(*start);
            let right = range.end.min(start + segment.len());
            if left < right {
                segments.push(segment.start + (left - start)..segment.start + (right - start));
            }
        }
        Self::new(source, segments)
    }
}
