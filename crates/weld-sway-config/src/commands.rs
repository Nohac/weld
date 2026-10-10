//! Comma-separated command spelling for window-rule interpreters.

use anyhow::{Result, anyhow};
use winnow::{
    Parser, Result as ParseResult,
    ascii::{space0, space1},
    combinator::{alt, delimited, repeat, separated, terminated},
    token::{any, take_while},
};

/// Split commands at unquoted commas, retaining each argument's original spelling.
///
/// # Errors
/// Rejects empty commands, unmatched quotes and unquoted semicolons.
pub fn parse(source: &str) -> Result<Vec<Vec<&str>>> {
    delimited(space0, separated(1.., command, (',', space0)), space0)
        .parse(source)
        .map_err(|error| anyhow!("invalid window-rule command list: {error}"))
}

fn command<'a>(input: &mut &'a str) -> ParseResult<Vec<&'a str>> {
    terminated(separated(1.., word, space1), space0).parse_next(input)
}

fn quoted<'a>(quote: char) -> impl Parser<&'a str, (), winnow::error::ContextError> {
    delimited(
        quote,
        repeat::<_, _, (), _, _>(
            0..,
            alt((
                ('\\', any).void(),
                any.verify(move |c: &char| *c != quote && *c != '\\').void(),
            )),
        ),
        quote,
    )
    .void()
}

fn word<'a>(input: &mut &'a str) -> ParseResult<&'a str> {
    repeat::<_, _, (), _, _>(
        1..,
        alt((
            ('\\', any).void(),
            quoted('"'),
            quoted('\''),
            take_while(1.., |c: char| {
                !c.is_whitespace() && !matches!(c, ',' | ';' | '"' | '\'' | '\\')
            })
            .void(),
        )),
    )
    .take()
    .parse_next(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_preserve_quoted_and_escaped_separators() {
        assert_eq!(
            parse(r#"floating enable,move to workspace "2: a,b", move absolute position 10 20"#)
                .expect("commands"),
            vec![
                vec!["floating", "enable"],
                vec!["move", "to", "workspace", "\"2: a,b\""],
                vec!["move", "absolute", "position", "10", "20"],
            ]
        );
        assert_eq!(
            parse(r"exec a\,b").expect("escape"),
            vec![vec!["exec", r"a\,b"]]
        );
        for invalid in [
            "",
            ",floating enable",
            "floating enable,",
            "floating enable,,border none",
            "move to workspace \"unfinished",
            "floating enable; border none",
        ] {
            assert!(parse(invalid).is_err(), "{invalid}");
        }
    }
}
