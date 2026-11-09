use super::*;

#[test]
fn generated_values_enforce_constraints_before_reaching_consumers() {
    assert!(ModeName::new(String::new()).is_err());
    assert!(BorderWidth::new(65).is_err());
    assert_eq!(BorderWidth::new(64).expect("maximum width").value(), 64);
    assert_eq!(
        ModeName::new("resize".into()).expect("mode").as_str(),
        "resize"
    );
    let mode = ModeHeader::words(&["--pango_markup", "\"a mode\""]).expect("mode header");
    assert!(matches!(mode, ModeHeader::Markup(name) if name.as_str() == "a mode"));
    assert!(ModeHeader::words(&["resize", "extra"]).is_err());
}

#[test]
fn mode_blocks_and_switches_have_distinct_typed_nodes_and_locations() {
    let document = parse(
        "modes.conf",
        "set $mode resize\nmode --pango_markup $mode {\n bindsym Escape mode default\n}\n",
        UnsupportedPolicy::Reject,
    )
    .expect("document");
    assert!(document.warnings.is_empty());
    let node = &document.directives[0];
    assert_eq!(node.location.source, "modes.conf");
    assert_eq!(node.location.line, 2);
    let Directive::Mode(mode) = &node.value else {
        panic!("expected mode definition")
    };
    assert!(matches!(&mode.header, ModeHeader::Markup(name) if name.as_str() == "resize"));
    assert_eq!(mode.bindings[0].location.line, 3);
    assert!(
        matches!(&mode.bindings[0].value.command, Command::Mode(name) if name.as_str() == "default")
    );
}

#[test]
fn input_lists_and_empty_options_have_their_own_lexical_contract() {
    for spelling in ["us,de", "\"us,de\"", "'us,de'"] {
        assert_eq!(XkbValue::word(spelling).expect("layout list").0, "us,de");
    }
    assert_eq!(XkbValue::word("\"\"").expect("clear options").0, "");
    for invalid in [
        "\"us",
        "us\"",
        "\"us\"suffix",
        "$missing",
        "us\\de",
        "us\0de",
    ] {
        assert!(XkbValue::word(invalid).is_err(), "{invalid:?}");
    }
    let document = parse("input.conf", "input type:keyboard {\n xkb_options ctrl:swapcaps,altwin:swap_lalt_lwin\n repeat_rate 30\n}\n", UnsupportedPolicy::Warn).expect("input");
    assert_eq!(document.directives.len(), 1);
    assert_eq!(document.warnings.len(), 1);
    assert_eq!(document.warnings[0].line, 3);
}

#[test]
fn output_skipping_and_shell_payloads_preserve_existing_interpretation() {
    let source = "output * pos 0 0 scale 1.25 adaptive_sync on\nexec sh -c 'printf \"a,b; c\"'\n";
    let document = parse("output.conf", source, UnsupportedPolicy::Warn).expect("document");
    assert_eq!(document.warnings.len(), 2);
    assert!(
        matches!(&document.directives[0].value, Directive::OutputScale(selector, factor) if selector.0 == "*" && factor.0 == 1.25)
    );
    assert!(
        matches!(&document.directives[1].value, Directive::Startup { command, on_reload: false } if command.0 == "sh -c 'printf \"a,b; c\"'")
    );
}

#[test]
fn rule_commands_retain_typed_targeting_and_quoted_commas() {
    let document = parse(
        "rules.conf",
        "for_window [title=\"a,b\"] floating enable, move to workspace \"2: a,b\"\n",
        UnsupportedPolicy::Reject,
    )
    .expect("rules");
    let Directive::Rule(rule) = &document.directives[0].value else {
        panic!("expected rule")
    };
    assert_eq!(rule.criteria[0].pattern, "a,b");
    let RuleEffect::Commands(commands) = &rule.effect else {
        panic!("expected ordered actions")
    };
    assert!(matches!(commands[0], Command::Floating(Toggle::Enable)));
    assert!(
        matches!(&commands[1], Command::MoveWorkspace(WorkspaceTarget::Name(name)) if name.as_str() == "2: a,b")
    );
    assert!(
        parse(
            "rules.conf",
            "for_window [all] floating enable; border none",
            UnsupportedPolicy::Warn
        )
        .is_err()
    );
}
