//! Sway configuration spelling and structural diagnostics, without a compositor.
//!
//! [`parse`] recognizes statements, arguments and multiline blocks. It preserves
//! quotes, escapes, criteria and command-chain punctuation rather than assigning
//! them meaning. Unknown directives, `set`, and `include` are ordinary statements:
//! parsing performs no expansion, file access, regex compilation or execution.
//! There is no compositor consumer yet, and success claims no supported WM behavior.
//!
//! # Syntax boundary
//!
//! The reference is Sway **1.12**, specifically `read_config`/`getline_with_cont`
//! in <https://github.com/swaywm/sway/blob/1.12/sway/config.c>, `config_command`
//! in <https://github.com/swaywm/sway/blob/1.12/sway/commands.c>, and `split_args`
//! in <https://github.com/swaywm/sway/blob/1.12/common/stringop.c>.
//!
//! * Whitespace is ASCII space, tab, LF, CR, form feed and vertical tab.
//! * Only whole logical lines starting with `#` after trimming are comments.
//!   Inline `#` characters, including colors, remain arguments.
//! * Backslash immediately followed by LF joins physical lines without adding a
//!   space, except when the logical line starts with `#` in column zero. Thus an
//!   indented comment can consume the following physical line. CRLF ends a line
//!   but backslash-CRLF does **not** continue it.
//! * Quotes and a single, non-nesting bracket group protect argument whitespace.
//!   Unclosed quotes/brackets remain raw spelling, as in Sway's argument splitter.
//! * A final bare `{` argument opens a block. A lone `{` on a following line also
//!   opens the preceding statement, skipping blank physical lines but not comments
//!   or intervening continuations (even ones collapsing to an empty logical line).
//! * Blocks are retained, not flattened into command prefixes. Semicolons and
//!   commas are not split at the config-reading stage.
//!
//! Weld deliberately requires a closing `}` to stand alone, rather than silently
//! discarding preceding text as Sway does. Unclosed blocks and excessive nesting
//! are errors; [`MAX_BLOCK_DEPTH`] is a Weld limit. This is not a round-trip
//! formatter: comments and leading/trailing statement whitespace are omitted.
//! Locations refer to original physical text, not Sway's final continued-line
//! number. Later expansion must re-tokenize expanded text; this unexpanded
//! spelling tree is not a fully resolved command AST.

#![deny(missing_docs)]

mod diagnostic;
mod parser;
mod syntax;

pub use diagnostic::{Diagnostic, DiagnosticKind};
pub use syntax::{Block, ParsedConfig, RawText, Statement};

/// Maximum simultaneous open blocks, bounding retained-tree depth and traversal.
pub const MAX_BLOCK_DEPTH: usize = 64;

/// Parses one in-memory config, using `source_name` only to label diagnostics.
///
/// The result owns its spellings; segment offsets always address `source`.
/// No environment, filesystem or compositor state is consulted.
///
/// # Errors
///
/// Returns the first structural diagnostic. Unknown commands and unresolved
/// variables are not structural errors. See the crate documentation for scope.
pub fn parse(source_name: &str, source: &str) -> Result<ParsedConfig, Diagnostic> {
    parser::parse(source_name, source)
}

#[cfg(test)]
mod tests;
