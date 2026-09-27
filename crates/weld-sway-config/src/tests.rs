use std::fmt::Write;

use crate::{DiagnosticKind, MAX_BLOCK_DEPTH, Statement, parse};

// Snapshot the public spelling model, not Winnow internals or serialization.
fn outline(source: &str) -> String {
    fn append(output: &mut String, statements: &[Statement], depth: usize) {
        for statement in statements {
            let words = std::iter::once(statement.name())
                .chain(statement.arguments())
                .map(|word| word.text())
                .collect::<Vec<_>>();
            writeln!(
                output,
                "{}{:?} => {:?}{}",
                "  ".repeat(depth),
                statement.header().text(),
                words,
                if statement.block().is_some() {
                    " {"
                } else {
                    ""
                },
            )
            .expect("writing to a string");
            if let Some(block) = statement.block() {
                append(output, block.statements(), depth + 1);
                writeln!(output, "{}}}", "  ".repeat(depth)).expect("writing to a string");
            }
        }
    }
    let config = parse("example.conf", source).expect("valid config structure");
    let mut output = String::new();
    append(&mut output, config.statements(), 0);
    output
}

#[test]
fn directives_remain_unexpanded_and_hash_is_not_an_inline_comment() {
    insta::assert_snapshot!(outline(r##"
# Whole-line comment
set $accent #aabbcc
include "./parts/*.conf"
future_option $accent # literal
"##), @r###"
    "set $accent #aabbcc" => ["set", "$accent", "#aabbcc"]
    "include \"./parts/*.conf\"" => ["include", "\"./parts/*.conf\""]
    "future_option $accent # literal" => ["future_option", "$accent", "#", "literal"]
    "###);
}

#[test]
fn unknown_blocks_and_next_line_braces_retain_structure() {
    insta::assert_snapshot!(outline(r##"panel {
  palette {
    accent "#aabbcc"
  }
}
mode "arrange"

{
  bindsym Mod1+x split v; focus right
}
"##), @r###"
    "panel" => ["panel"] {
      "palette" => ["palette"] {
        "accent \"#aabbcc\"" => ["accent", "\"#aabbcc\""]
      }
    }
    "mode \"arrange\"" => ["mode", "\"arrange\""] {
      "bindsym Mod1+x split v; focus right" => ["bindsym", "Mod1+x", "split", "v;", "focus", "right"]
    }
    "###);
}

#[test]
fn quoting_criteria_and_shell_punctuation_are_preserved() {
    insta::assert_snapshot!(outline(r##"bindsym Mod1+y exec sh -c 'printf "%s" "{x}; # kept"'
for_window [app_id="org.example.Editor" title="a,b; c"] floating enable, focus
exec printf escaped\ space
example "{" bar{
"##), @r###"
    "bindsym Mod1+y exec sh -c 'printf \"%s\" \"{x}; # kept\"'" => ["bindsym", "Mod1+y", "exec", "sh", "-c", "'printf \"%s\" \"{x}; # kept\"'"]
    "for_window [app_id=\"org.example.Editor\" title=\"a,b; c\"] floating enable, focus" => ["for_window", "[app_id=\"org.example.Editor\" title=\"a,b; c\"]", "floating", "enable,", "focus"]
    "exec printf escaped\\ space" => ["exec", "printf", "escaped\\ space"]
    "example \"{\" bar{" => ["example", "\"{\"", "bar{"]
    "###);
}

#[test]
fn continued_tokens_keep_exact_physical_source_segments() {
    let source = "set $name ab\\\ncd\n";
    let config = parse("continued.conf", source).expect("continued token");
    let statement = &config.statements()[0];
    let value = &statement.arguments()[1];
    insta::assert_snapshot!(outline(source), @r###""set $name abcd" => ["set", "$name", "abcd"]"###);
    assert_eq!(value.segments(), &[10..12, 14..16]);
    assert_eq!(statement.header().segments(), &[0..12, 14..16]);
    for text in std::iter::once(statement.header())
        .chain(std::iter::once(statement.name()))
        .chain(statement.arguments())
    {
        let reconstructed = text
            .segments()
            .iter()
            .map(|range| &source[range.clone()])
            .collect::<String>();
        assert_eq!(text.text(), reconstructed);
    }
}

#[test]
fn line_endings_and_raw_comment_continuation_guard_match_sway() {
    let source = "# column zero \\\nkeep first\n  # indented \\\nswallowed yes\nkeep second\r\nkeep third \\\r\nkeep fourth\n\\\n# new column zero \\\nkeep fifth\n";
    insta::assert_snapshot!(outline(source), @r###"
    "keep first" => ["keep", "first"]
    "keep second" => ["keep", "second"]
    "keep third \\" => ["keep", "third", "\\"]
    "keep fourth" => ["keep", "fourth"]
    "keep fifth" => ["keep", "fifth"]
    "###);
}

#[test]
fn non_ascii_spaces_and_unclosed_argument_delimiters_remain_spelling() {
    let source =
        "example alpha\u{a0}beta\nexample \"unfinished value\nexample [title=unfinished value";
    insta::assert_snapshot!(outline(source), @r###"
    "example alpha\u{a0}beta" => ["example", "alpha\u{a0}beta"]
    "example \"unfinished value" => ["example", "\"unfinished value"]
    "example [title=unfinished value" => ["example", "[title=unfinished value"]
    "###);
}

#[test]
fn structural_errors_show_the_original_line_and_column() {
    let mut output = String::new();
    for source in [
        "}",
        "{",
        "panel {\n value yes }\n",
        "panel {\n value yes\n",
        "panel\n# comment stops brace lookahead\n{\n}\n",
        "example é }",
        "panel {\n value yes \\\n }\n",
        "panel\n\\\n{\n}\n",
        "panel\n{\\\n\n}\n",
        "panel\n\\\n\n{\n",
        "panel { value yes }",
    ] {
        let error = parse("example.conf", source).expect_err("invalid block structure");
        writeln!(output, "{error}\n").expect("writing to a string");
    }
    insta::assert_snapshot!(output, @r###"
    example.conf:1:1: closing brace has no open block
    1 | }
      | ^

    example.conf:1:1: opening brace has no preceding statement
    1 | {
      | ^

    example.conf:2:12: closing brace must stand alone; Weld does not discard preceding command text
    2 |  value yes }
      |            ^

    example.conf:1:7: block is not closed before end of source
    1 | panel {
      |       ^

    example.conf:3:1: opening brace has no preceding statement
    3 | {
      | ^

    example.conf:1:11: closing brace must stand alone; Weld does not discard preceding command text
    1 | example é }
      |           ^

    example.conf:3:2: closing brace must stand alone; Weld does not discard preceding command text
    3 |  }
      |  ^

    example.conf:3:1: opening brace has no preceding statement
    3 | {
      | ^

    example.conf:2:1: opening brace has no preceding statement
    2 | {\
      | ^

    example.conf:4:1: opening brace has no preceding statement
    4 | {
      | ^

    example.conf:1:19: closing brace must stand alone; Weld does not discard preceding command text
    1 | panel { value yes }
      |                   ^
    "###);

    let error = parse("unicode.conf", "example é }").expect_err("closing content");
    assert_eq!(
        (error.line(), error.column(), error.span()),
        (1, 11, 11..12)
    );
    assert_eq!(error.source_name(), "unicode.conf");

    let continued_header =
        parse("example.conf", "panel \\\n{\n}\n").expect("continued header opens a block");
    assert!(continued_header.statements()[0].block().is_some());
}

#[test]
fn nesting_is_bounded_and_brace_locations_are_retained() {
    let source = format!(
        "{}{}",
        "panel {\n".repeat(MAX_BLOCK_DEPTH),
        "}\n".repeat(MAX_BLOCK_DEPTH)
    );
    let config = parse("depth.conf", &source).expect("maximum accepted depth");
    let first = config.statements()[0].block().expect("outer block");
    assert_eq!(first.opening(), 6..7);
    assert_eq!(first.closing(), source.len() - 2..source.len() - 1);
    let excessive = format!("panel {{\n{source}");
    let error = parse("depth.conf", &excessive).expect_err("depth bound");
    assert_eq!(error.kind(), DiagnosticKind::NestingLimit);
    assert_eq!(error.line(), MAX_BLOCK_DEPTH + 1);
    assert_eq!(
        error.kind().to_string(),
        format!("block nesting exceeds Weld's limit of {MAX_BLOCK_DEPTH}")
    );
}

#[test]
fn empty_source_and_ascii_whitespace_have_no_statements() {
    for source in ["", " \t\r\n\x0b\x0c", "# comment without newline"] {
        assert!(
            parse("empty.conf", source)
                .expect("empty structure")
                .statements()
                .is_empty()
        );
    }
    let config = parse("space.conf", "example a\x0cb\x0bc").expect("ASCII argument separators");
    assert_eq!(
        config.statements()[0]
            .arguments()
            .iter()
            .map(|argument| argument.text())
            .collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
    for source in [r"example trailing \", "example trailing \\\n"] {
        let config = parse("eof.conf", source).expect("literal backslash without a successor line");
        assert_eq!(config.statements()[0].arguments()[1].text(), "\\");
    }
}
