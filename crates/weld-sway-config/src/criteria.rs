//! Lexical criteria fields for configuration interpreters.

use anyhow::{Result, anyhow};
use winnow::{
    Parser, Result as ParseResult,
    combinator::{alt, delimited, opt, preceded, repeat},
    token::{any, take_while},
};

/// A criterion's spelling, with quoted values unescaped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Criterion {
    /// Field name, interpreted by the configuration consumer.
    pub name: String,
    /// Optional value; flags such as `all` have none.
    pub value: Option<String>,
}

/// Parse a complete bracketed criteria expression.
///
/// # Errors
/// Rejects malformed fields, missing brackets and unterminated quotes.
pub fn parse(source: &str) -> Result<Vec<Criterion>> {
    delimited(
        '[',
        delimited(whitespace, repeat(1.., field), whitespace),
        ']',
    )
    .parse(source)
    .map_err(|error| anyhow!("invalid window criteria: {error}"))
}

fn field(input: &mut &str) -> ParseResult<Criterion> {
    let name: &str =
        take_while(1.., |c: char| c.is_ascii_alphanumeric() || c == '_').parse_next(input)?;
    whitespace.parse_next(input)?;
    let value = opt(preceded(('=', whitespace), value)).parse_next(input)?;
    whitespace.parse_next(input)?;
    Ok(Criterion {
        name: name.to_owned(),
        value,
    })
}

fn whitespace<'a>(input: &mut &'a str) -> ParseResult<&'a str> {
    take_while(0.., |c: char| c.is_ascii_whitespace()).parse_next(input)
}

fn value(input: &mut &str) -> ParseResult<String> {
    alt((
        delimited(
            '"',
            repeat(
                0..,
                alt(("\\\"".value('"'), any.verify(|c: &char| *c != '"'))),
            ),
            '"',
        ),
        take_while(1.., |c: char| !c.is_whitespace() && !matches!(c, ']' | '"')).map(str::to_owned),
    ))
    .parse_next(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn criteria_preserve_regex_escapes_and_quoted_whitespace() {
        let fields =
            parse(r#"[class = "^Demo\d+$" title="A \"quoted\" title"]"#).expect("criteria");
        assert_eq!(fields[0].value.as_deref(), Some(r"^Demo\d+$"));
        assert_eq!(fields[1].value.as_deref(), Some("A \"quoted\" title"));
        assert_eq!(
            parse(r#"[title="a\\b"]"#).expect("literal slash")[0]
                .value
                .as_deref(),
            Some(r"a\\b")
        );
        assert_eq!(parse("[all]").expect("all")[0].value, None);
        for invalid in ["[]", "[title=]", "[title=\"unfinished]", "[all] trailing"] {
            assert!(parse(invalid).is_err(), "{invalid}");
        }
    }
}
