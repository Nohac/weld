//! Literal values shared by subsystem translators.

use winnow::{
    Parser, Result,
    combinator::{alt, delimited},
    token::take_while,
};

pub(crate) fn literal<'a>(input: &mut &'a str) -> Result<&'a str> {
    alt((
        delimited(
            '"',
            take_while(0.., |c: char| c != '"' && literal_character(c)),
            '"',
        ),
        delimited(
            '\'',
            take_while(0.., |c: char| c != '\'' && literal_character(c)),
            '\'',
        ),
        take_while(1.., |c: char| {
            literal_character(c) && !c.is_whitespace() && !matches!(c, '"' | '\'' | ',' | ';')
        }),
    ))
    .parse_next(input)
}

fn literal_character(c: char) -> bool {
    !c.is_control() && !matches!(c, '\\' | '$')
}

pub(crate) fn parse(value: &str) -> anyhow::Result<&str> {
    literal
        .parse(value)
        .map_err(|error| anyhow::anyhow!("invalid literal argument: {error}"))
}
