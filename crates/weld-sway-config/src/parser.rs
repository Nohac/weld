//! Physical line assembly and unexpanded argument scanning.

use std::ops::Range;

use winnow::{
    Parser, Result as ParseResult,
    combinator::{alt, opt, repeat},
    token::{any, take_till, take_while},
};

use crate::{Block, Diagnostic, DiagnosticKind, MAX_BLOCK_DEPTH, ParsedConfig, RawText, Statement};

struct Word {
    logical_range: Range<usize>,
    raw: RawText,
}

struct LogicalLine {
    spelling: RawText,
    had_continuation: bool,
}

struct OpenBlock {
    statement: Statement,
    opening: Range<usize>,
    children: Vec<Statement>,
}

/// Classified spelling, without access to the in-progress block tree.
enum LineKind {
    Blank,
    LookaheadBarrier,
    Statement {
        statement: Statement,
        allows_next_line_brace: bool,
    },
    BlockHeader {
        statement: Statement,
        opening: Range<usize>,
    },
    OpenPrevious {
        opening: Range<usize>,
    },
    CloseBlock {
        closing: Range<usize>,
    },
}

pub(crate) fn parse(source_name: &str, source: &str) -> Result<ParsedConfig, Diagnostic> {
    let mut blocks = BlockAssembler::new(source_name, source);
    for line in logical_lines(source) {
        blocks.accept(classify_line(source_name, source, line)?)?;
    }
    blocks.finish()
}

/// Owns Sway's line/brace recognition rules, including physical-line barriers.
fn classify_line(
    source_name: &str,
    source: &str,
    line: LogicalLine,
) -> Result<LineKind, Diagnostic> {
    let LogicalLine {
        spelling: line,
        had_continuation,
    } = line;
    if line.text().is_empty() && !had_continuation {
        return Ok(LineKind::Blank);
    }
    if line.text().is_empty() || line.text().starts_with('#') {
        return Ok(LineKind::LookaheadBarrier);
    }

    let mut words = arguments(source_name, source, &line)?;
    let Some(last) = words.last() else {
        return Ok(LineKind::Blank);
    };
    let brace_span = envelope(&last.raw);
    let opening = match (last.raw.text(), words.len()) {
        ("}", 1) => {
            return Ok(LineKind::CloseBlock {
                closing: brace_span,
            });
        }
        ("}", _) => {
            return Err(Diagnostic::new(
                source_name,
                source,
                brace_span,
                DiagnosticKind::ContentBeforeClosingBrace,
            ));
        }
        ("{", 1) if had_continuation => {
            return Err(Diagnostic::new(
                source_name,
                source,
                brace_span,
                DiagnosticKind::UnexpectedOpeningBrace,
            ));
        }
        ("{", 1) => {
            return Ok(LineKind::OpenPrevious {
                opening: brace_span,
            });
        }
        ("{", _) => {
            words.pop();
            Some(brace_span.clone())
        }
        _ => None,
    };
    let statement = statement(source, &line, words).ok_or_else(|| {
        Diagnostic::new(
            source_name,
            source,
            brace_span,
            DiagnosticKind::UnexpectedOpeningBrace,
        )
    })?;
    Ok(match opening {
        Some(opening) => LineKind::BlockHeader { statement, opening },
        None => LineKind::Statement {
            statement,
            // Sway does not look ahead when the trimmed line ends in either
            // brace, even when that brace is merely part of an ordinary word.
            allows_next_line_brace: !line.text().ends_with(['{', '}']),
        },
    })
}

/// Owns tree mutations and pending-header eligibility, not lexical syntax.
///
/// Eligibility always refers to the last statement in the current container.
/// Opening or closing a block clears it before that container can change.
struct BlockAssembler<'a> {
    source_name: &'a str,
    source: &'a str,
    roots: Vec<Statement>,
    stack: Vec<OpenBlock>,
    previous_can_open: bool,
}

impl<'a> BlockAssembler<'a> {
    fn new(source_name: &'a str, source: &'a str) -> Self {
        Self {
            source_name,
            source,
            roots: Vec::new(),
            stack: Vec::new(),
            previous_can_open: false,
        }
    }

    fn accept(&mut self, line: LineKind) -> Result<(), Diagnostic> {
        match line {
            LineKind::Blank => {}
            LineKind::LookaheadBarrier => self.previous_can_open = false,
            LineKind::Statement {
                statement,
                allows_next_line_brace,
            } => {
                self.children().push(statement);
                self.previous_can_open = allows_next_line_brace;
            }
            LineKind::BlockHeader { statement, opening } => self.open(statement, opening)?,
            LineKind::OpenPrevious { opening } => self.open_previous(opening)?,
            LineKind::CloseBlock { closing } => self.close(closing)?,
        }
        Ok(())
    }

    fn open(&mut self, statement: Statement, opening: Range<usize>) -> Result<(), Diagnostic> {
        if self.stack.len() == MAX_BLOCK_DEPTH {
            return Err(self.error(opening, DiagnosticKind::NestingLimit));
        }
        self.previous_can_open = false;
        self.stack.push(OpenBlock {
            statement,
            opening,
            children: Vec::new(),
        });
        Ok(())
    }

    fn open_previous(&mut self, opening: Range<usize>) -> Result<(), Diagnostic> {
        if !self.previous_can_open {
            return Err(self.error(opening, DiagnosticKind::UnexpectedOpeningBrace));
        }
        let statement = self
            .children()
            .pop()
            .ok_or_else(|| self.error(opening.clone(), DiagnosticKind::UnexpectedOpeningBrace))?;
        self.open(statement, opening)
    }

    fn close(&mut self, closing: Range<usize>) -> Result<(), Diagnostic> {
        let mut block = self
            .stack
            .pop()
            .ok_or_else(|| self.error(closing.clone(), DiagnosticKind::UnexpectedClosingBrace))?;
        self.previous_can_open = false;
        block.statement.block = Some(Block {
            opening: block.opening,
            closing,
            statements: block.children,
        });
        self.children().push(block.statement);
        Ok(())
    }

    fn children(&mut self) -> &mut Vec<Statement> {
        match self.stack.last_mut() {
            Some(block) => &mut block.children,
            None => &mut self.roots,
        }
    }

    fn finish(self) -> Result<ParsedConfig, Diagnostic> {
        if let Some(block) = self.stack.last() {
            return Err(self.error(block.opening.clone(), DiagnosticKind::UnclosedBlock));
        }
        Ok(ParsedConfig {
            statements: self.roots,
        })
    }

    fn error(&self, span: Range<usize>, kind: DiagnosticKind) -> Diagnostic {
        Diagnostic::new(self.source_name, self.source, span, kind)
    }
}

fn statement(source: &str, line: &RawText, words: Vec<Word>) -> Option<Statement> {
    let end = words.last()?.logical_range.end;
    let mut words = words.into_iter();
    let first = words.next()?;
    Some(Statement {
        header: line.slice(source, first.logical_range.start..end),
        name: first.raw,
        arguments: words.map(|word| word.raw).collect(),
        block: None,
    })
}

fn envelope(text: &RawText) -> Range<usize> {
    match (text.segments().first(), text.segments().last()) {
        (Some(first), Some(last)) => first.start..last.end,
        _ => 0..0,
    }
}

fn is_space(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

fn logical_lines(source: &str) -> Vec<LogicalLine> {
    let mut physical = source.split_inclusive('\n').peekable();
    let mut offset = 0;
    let mut lines = Vec::new();
    while let Some(mut part) = physical.next() {
        let mut column_zero_comment = part.starts_with('#');
        let mut segments = Vec::new();
        let mut had_continuation = false;
        loop {
            let continued =
                !column_zero_comment && part.ends_with("\\\n") && physical.peek().is_some();
            let end = if continued {
                part.len() - 2
            } else {
                part.strip_suffix('\n').unwrap_or(part).len()
            };
            if end != 0 {
                segments.push(offset..offset + end);
            }
            offset += part.len();
            if continued && let Some(next) = physical.next() {
                had_continuation = true;
                if segments.is_empty() {
                    // An empty continued prefix lets the next physical line
                    // become column zero of the assembled line.
                    column_zero_comment = next.starts_with('#');
                }
                part = next;
                continue;
            }
            break;
        }
        let line = RawText::new(source, segments);
        let start = line.text().len() - line.text().trim_start_matches(is_space).len();
        let end = line.text().trim_end_matches(is_space).len().max(start);
        lines.push(LogicalLine {
            spelling: line.slice(source, start..end),
            had_continuation,
        });
    }
    lines
}

fn arguments(source_name: &str, source: &str, line: &RawText) -> Result<Vec<Word>, Diagnostic> {
    let mut remaining = line.text();
    let mut words = Vec::new();
    while !remaining.is_empty() {
        remaining = remaining.trim_start_matches(is_space);
        if remaining.is_empty() {
            break;
        }
        let start = line.text().len() - remaining.len();
        if argument.parse_next(&mut remaining).is_err() {
            let location = line.slice(source, start..line.text().len());
            return Err(Diagnostic::new(
                source_name,
                source,
                envelope(&location),
                DiagnosticKind::InvalidArgument,
            ));
        }
        let range = start..line.text().len() - remaining.len();
        words.push(Word {
            raw: line.slice(source, range.clone()),
            logical_range: range,
        });
    }
    Ok(words)
}

fn escaped(input: &mut &str) -> ParseResult<()> {
    ('\\', opt(any)).void().parse_next(input)
}

fn quoted<'a>(mut delimiter: char) -> impl Parser<&'a str, (), winnow::error::ContextError> {
    move |input: &mut &'a str| {
        delimiter.parse_next(input)?;
        let _: () = repeat(
            0..,
            alt((escaped, take_till(1.., [delimiter, '\\']).void())),
        )
        .parse_next(input)?;
        opt(delimiter).parse_next(input)?;
        Ok(())
    }
}

fn bracketed(input: &mut &str) -> ParseResult<()> {
    '['.parse_next(input)?;
    let _: () = repeat(
        0..,
        alt((
            escaped,
            quoted('"'),
            quoted('\''),
            take_till(1.., ['\\', '"', '\'', ']']).void(),
        )),
    )
    .parse_next(input)?;
    opt(']').parse_next(input)?;
    Ok(())
}

fn argument<'a>(input: &mut &'a str) -> ParseResult<&'a str> {
    repeat::<_, _, (), _, _>(
        1..,
        alt((
            escaped,
            quoted('"'),
            quoted('\''),
            bracketed,
            take_while(1.., |character: char| {
                !is_space(character) && !matches!(character, '\\' | '"' | '\'' | '[')
            })
            .void(),
        )),
    )
    .take()
    .parse_next(input)
}
