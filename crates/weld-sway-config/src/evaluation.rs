//! Source-ordered variables and explicit unsupported-feature diagnostics.
//!
//! Expansion uses the parser's logical headers and reparses the resulting source.
//! Physical line counts survive continuation removal for downstream diagnostics.

use std::{cmp::Reverse, error::Error, fmt};

use anyhow::{Context, Result, ensure};

use crate::{Statement, parse};

const MAX_EXPANDED_BYTES: usize = 8 * 1024 * 1024;

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

/// Physical line containing the beginning of a parsed statement.
/// Returns an error if the statement's source offset is outside this source.
pub fn line_of(source: &str, statement: &Statement) -> Result<usize> {
    let offset = statement
        .header()
        .segments()
        .first()
        .map_or(0, |span| span.start);
    Ok(source
        .get(..offset)
        .context("statement offset is outside its source")?
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1)
}

/// Expand top-level `set` definitions and references in source order.
///
/// Longer names take precedence, values are substituted once, and redefining a
/// name affects subsequent statements only. Undefined references survive for
/// shell execution or a downstream literal/chord diagnostic. `$$` protects a
/// dollar sign; backslash-escaped dollars remain escaped. Quotes are retained.
/// No environment lookup, file access or command execution occurs.
///
/// # Errors
/// Returns structural errors, malformed definitions, or expansion size overflow.
pub fn expand_variables(name: &str, source: &str) -> Result<String> {
    ensure!(
        source.len() <= MAX_EXPANDED_BYTES,
        "{name}: configuration exceeds expansion size limit"
    );
    let syntax = parse(name, source)?;
    let mut expansion = Expansion {
        source,
        output: String::new(),
        cursor: 0,
        variables: Vec::new(),
    };
    for statement in syntax.statements() {
        let line = line_of(source, statement)?;
        expansion
            .statement(statement, true)
            .with_context(|| format!("{name}:{line}"))?;
    }
    expansion.append(&source[expansion.cursor..])?;
    if !source.is_empty() && !source.ends_with('\n') {
        expansion.append("\n")?;
    }
    Ok(expansion.output)
}

struct Expansion<'a> {
    source: &'a str,
    output: String,
    cursor: usize,
    variables: Vec<(String, String)>,
}

impl Expansion<'_> {
    fn append(&mut self, text: &str) -> Result<()> {
        ensure!(
            self.output.len().saturating_add(text.len()) <= MAX_EXPANDED_BYTES,
            "expanded configuration exceeds size limit"
        );
        self.output.push_str(text);
        Ok(())
    }

    fn statement(&mut self, statement: &Statement, top_level: bool) -> Result<()> {
        let segments = statement.header().segments();
        let (Some(first), Some(last)) = (segments.first(), segments.last()) else {
            return Ok(());
        };
        self.append(&self.source[self.cursor..first.start])?;
        let definition = top_level && statement.name().text() == "set";
        let replacement = if definition {
            ensure!(
                statement.block().is_none(),
                "set requires a name and value, not a block"
            );
            let [variable, values @ ..] = statement.arguments() else {
                anyhow::bail!("set requires a name and value")
            };
            ensure!(
                variable.text().starts_with('$') && variable.text().len() > 1 && !values.is_empty(),
                "set requires a $name and value"
            );
            let value = values
                .iter()
                .map(|value| value.text())
                .collect::<Vec<_>>()
                .join(" ");
            let value = substitute(&value, &self.variables)?;
            self.variables
                .retain(|(existing, _)| existing != variable.text());
            self.variables.push((variable.text().into(), value));
            ensure!(
                self.variables
                    .iter()
                    .map(|(name, value)| name.len().saturating_add(value.len()))
                    .sum::<usize>()
                    <= MAX_EXPANDED_BYTES,
                "variable table exceeds size limit"
            );
            self.variables.sort_by_key(|(name, _)| Reverse(name.len()));
            String::new()
        } else {
            substitute(statement.header().text(), &self.variables)?
        };
        self.append(&replacement)?;
        // Keep subsequent physical lines stable even when a header spanned
        // several continued lines. Definitions become blank physical lines.
        for _ in self.source[first.start..last.end]
            .bytes()
            .filter(|byte| *byte == b'\n')
        {
            self.append(if definition { "\n" } else { "\\\n" })?;
        }
        self.cursor = last.end;
        if let Some(block) = statement.block() {
            for child in block.statements() {
                self.statement(child, false)?;
            }
        }
        Ok(())
    }
}

fn substitute(text: &str, variables: &[(String, String)]) -> Result<String> {
    let mut output = String::new();
    let mut cursor = 0;
    while cursor < text.len() {
        let remaining = &text[cursor..];
        let prefix = output.as_bytes();
        let escaped = prefix.ends_with(b"\\") && !prefix.ends_with(b"\\\\");
        let (value, consumed) = if remaining.starts_with('$') && !escaped {
            if remaining.starts_with("$$") {
                ("$", 2)
            } else if let Some((name, value)) = variables
                .iter()
                .find(|(name, _)| remaining.starts_with(name))
            {
                (value.as_str(), name.len())
            } else {
                ("$", 1)
            }
        } else {
            let Some(character) = remaining.chars().next() else {
                break;
            };
            (&remaining[..character.len_utf8()], character.len_utf8())
        };
        ensure!(
            output.len().saturating_add(value.len()) <= MAX_EXPANDED_BYTES,
            "variable expansion exceeds size limit"
        );
        output.push_str(value);
        cursor += consumed;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continued_header_at_eof_reparses_without_a_spurious_backslash() {
        let text =
            expand_variables("eof", "set $mod Mod4\nbindsym $mod+x exec a \\\nb").expect("expand");
        let syntax = parse("eof", &text).expect("syntax");
        assert_eq!(
            syntax.statements()[0].header().text(),
            "bindsym Mod4+x exec a b"
        );
        assert_eq!(line_of(&text, &syntax.statements()[0]).expect("line"), 2);
    }

    #[test]
    fn source_mismatch_is_an_error_and_replaced_prefix_controls_dollar_escaping() {
        let syntax = parse("long", "\n\n\nfont value").expect("syntax");
        assert!(line_of("x", &syntax.statements()[0]).is_err());
        let variables = vec![
            ("$slash".into(), "\\".into()),
            ("$mod".into(), "Mod4".into()),
        ];
        assert_eq!(
            substitute("$slash$mod", &variables).expect("expand"),
            "\\$mod"
        );
        assert_eq!(
            substitute("$$ $$mod $$$mod", &variables).expect("expand"),
            "$ $mod $Mod4"
        );
    }

    #[test]
    fn definitions_retokenize_in_order_and_longest_names_win() {
        let text = expand_variables("test", "set $mod Mod4\nset $modshift $mod+Shift\nset $launch exec foot\nbindsym $modshift+Return $launch\nset $mod Mod1\nfloating_modifier $mod\n").expect("expansion");
        let syntax = parse("test", &text).expect("expanded syntax");
        assert_eq!(
            syntax.statements()[0].header().text(),
            "bindsym Mod4+Shift+Return exec foot"
        );
        assert_eq!(
            syntax.statements()[1].header().text(),
            "floating_modifier Mod1"
        );
        assert_eq!(line_of(&text, &syntax.statements()[1]).expect("line"), 6);
    }

    #[test]
    fn quotes_dollars_and_continued_lines_keep_their_meaning() {
        let source = "set $name a\\\nb\nset $layout us\ninput type:keyboard {\n xkb_layout $layout\n}\nexec echo '$name' $$name \\$name $HOME\nset $forward $later\nset $later resolved\nexec echo $forward\n";
        let text = expand_variables("test", source).expect("expand");
        let syntax = parse("test", &text).expect("syntax");
        assert_eq!(
            syntax.statements()[0].block().expect("block").statements()[0]
                .header()
                .text(),
            "xkb_layout us"
        );
        assert_eq!(
            syntax.statements()[1].header().text(),
            "exec echo 'ab' $name \\$name $HOME"
        );
        assert_eq!(line_of(&text, &syntax.statements()[1]).expect("line"), 7);
        assert_eq!(syntax.statements()[2].header().text(), "exec echo $later");
    }

    #[test]
    fn malformed_definitions_and_exponential_growth_are_rejected() {
        for source in ["set mod Mod4", "set $mod", "set $ Mod4"] {
            assert!(expand_variables("invalid", source).is_err());
        }
        let source = "set $v value\n".to_owned() + &"set $v $v$v\n".repeat(24);
        assert!(expand_variables("bounded", &source).is_err());
    }
}
