//! Lexical grammar for input directive values.

use winnow::{
    Parser, Result as ParseResult,
    combinator::{alt, delimited, repeat, terminated},
    token::take_while,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Modifier<'a>(pub &'a str);

pub(super) fn chord<'a>(input: &mut &'a str) -> ParseResult<(Vec<Modifier<'a>>, &'a str)> {
    let modifiers = repeat(0.., terminated(chord_word.map(Modifier), '+')).parse_next(input)?;
    let key = chord_word.parse_next(input)?;
    Ok((modifiers, key))
}

fn chord_word<'a>(input: &mut &'a str) -> ParseResult<&'a str> {
    take_while(1.., |c: char| c != '+' && !c.is_whitespace()).parse_next(input)
}

pub(super) fn xkb_value<'a>(input: &mut &'a str) -> ParseResult<&'a str> {
    alt((
        delimited('"', take_while(0.., xkb_name_character), '"'),
        delimited('\'', take_while(0.., xkb_name_character), '\''),
        take_while(1.., xkb_name_character),
    ))
    .parse_next(input)
}

fn xkb_name_character(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ',' | ':' | '+' | '/' | '.' | '(' | ')')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_values_and_chords_require_complete_syntax() {
        assert_eq!(xkb_value.parse("\"\"").expect("empty options"), "");
        assert_eq!(xkb_value.parse("'us,de'").expect("layouts"), "us,de");
        for invalid in [
            "\"us",
            "us\"",
            "\"us\"suffix",
            "$layout",
            "us\\de",
            "us\0de",
        ] {
            assert!(xkb_value.parse(invalid).is_err(), "{invalid:?}");
        }
        let (modifiers, key) = chord.parse("Mod1+Control+f").expect("chord");
        assert_eq!(modifiers, [Modifier("Mod1"), Modifier("Control")]);
        assert_eq!(key, "f");
        assert_eq!(
            chord.parse("Hyper+f").expect("modifier spelling").0,
            [Modifier("Hyper")]
        );
        for invalid in ["Mod1+", "Mod1++f", "+f", "Mod1+ f"] {
            assert!(chord.parse(invalid).is_err(), "{invalid}");
        }
    }
}
