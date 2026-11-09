use super::*;
use weld_i3_config::{I3LayoutRequest, LayoutChoice};

#[test]
fn typed_lowering_preserves_native_values_and_rule_origin_without_reparsing() {
    let location = weld_config::Location {
        source: "typed.conf".into(),
        line: 7,
        range: 10..20,
    };
    let document = syntax::Document {
        directives: vec![
            Located {
                location: location.clone(),
                value: syntax::Directive::Setting(syntax::Setting::GapInner(
                    syntax::Gap::new(12).expect("gap"),
                )),
            },
            Located {
                location: location.clone(),
                value: syntax::Directive::Rule(syntax::WindowRule {
                    criteria: vec![syntax::Criterion {
                        field: syntax::CriterionField::AppId,
                        pattern: "^foot$".into(),
                    }],
                    effect: syntax::RuleEffect::Commands(vec![syntax::Command::Floating(
                        syntax::Toggle::Enable,
                    )]),
                }),
            },
        ],
        warnings: Vec::new(),
    };
    let config = lower::<()>(document, UnsupportedPolicy::Reject, |_| {
        bail!("unexpected extension")
    })
    .expect("lowering");
    assert_eq!(config.tiling.inner_gap, 12);
    assert_eq!(config.window_rules[0].location, location);
    assert!(
        matches!(&config.window_rules[0].value.effect, RuleEffect::Commands(actions) if actions == &[RuleAction::Floating(true)])
    );
}

#[test]
fn default_mode_workspace_bindings_contribute_to_initial_workspace_order() {
    let config = parse_with_extensions::<()>(
        "default-mode",
        "mode default {\n bindsym a workspace 3\n}\nbindsym b workspace 2\nmode default {\n bindsym c workspace 4\n}\n",
        |_| bail!("extension"),
    )
    .expect("config");
    assert_eq!(config.workspaces.initial_names, ["3", "2", "4"]);
}

#[test]
fn unresolved_mode_references_report_the_binding_source_line() {
    let error = parse_with_extensions::<()>(
        "mode.conf",
        "mode resize {\n bindsym a mode missing\n}\n",
        |_| bail!("extension"),
    )
    .expect_err("missing mode");
    assert!(format!("{error:#}").contains("mode.conf:2:"));
}

#[test]
fn output_moves_are_never_reinterpreted_as_workspace_names() {
    let config = parse_with_policy::<()>(
        "output-move",
        "bindsym a move workspace to output DP-1\nbindsym b move --no-auto-back-and-forth container workspace 2\n",
        UnsupportedPolicy::Warn,
        |_| bail!("extension"),
    ).expect("config");
    assert_eq!(config.warnings.len(), 1);
    assert_eq!(config.bindings.len(), 1);
    assert!(
        matches!(&config.bindings[0].1, Action::Workspace(I3WorkspaceRequest::Navigate {
        target: WorkspaceTarget::Name(name), operation: WorkspaceNavigation::MoveWindow,
        allow_auto_back_and_forth: false,
    }) if name == "2")
    );
}
#[cfg(test)]
mod layout_tests {
    use super::*;

    use weld_tile_config::TileLayout;

    #[test]
    fn layout_bindings_compile_to_distinct_preparation_and_layout_actions() {
        let config: Configuration = parse_with_extensions(
            "layouts",
            indoc::indoc! {"
            set $mod Mod1
            bindsym $mod+s layout stacking
            bindsym $mod+w layout tabbed
            bindsym $mod+e layout toggle split
            bindsym $mod+v splitv
            bindsym $mod+h layout splith
            bindsym $mod+t layout toggle tabbed stacked splitv
        "},
            |_| bail!("unexpected extension"),
        )
        .expect("config");
        assert_eq!(
            config
                .bindings
                .iter()
                .map(|(_, action)| action.clone())
                .collect::<Vec<_>>(),
            [
                Action::Layout(I3LayoutRequest::Set(TileLayout::Stacked)),
                Action::Layout(I3LayoutRequest::Set(TileLayout::Tabbed)),
                Action::Layout(I3LayoutRequest::Cycle(vec![LayoutChoice::Split])),
                Action::Tile(TileOperation::Split(SplitAxis::Vertical)),
                Action::Layout(I3LayoutRequest::Set(TileLayout::Split(
                    SplitAxis::Horizontal
                ))),
                Action::Layout(I3LayoutRequest::Cycle(vec![
                    LayoutChoice::Layout(TileLayout::Tabbed),
                    LayoutChoice::Layout(TileLayout::Stacked),
                    LayoutChoice::Layout(TileLayout::Split(SplitAxis::Vertical))
                ])),
            ]
        );
    }

    #[test]
    fn invalid_layout_cycle_rejects_the_candidate_instead_of_mutating_live_state() {
        let result: Result<Configuration> =
            parse_with_extensions("layouts", "bindsym Mod1+x layout toggle 1337 1337", |_| {
                bail!("extension")
            });
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod interpretation {
    use super::*;
    use indoc::indoc;

    fn parse(name: &str, source: &str) -> Result<Configuration> {
        parse_with_extensions(name, source, |_| bail!("unsupported i3 command"))
    }

    #[test]
    fn variables_configure_bindings_settings_and_exec_without_evaluating_shell() {
        let config = parse(
            "variables",
            indoc! {r#"
            set $mod Mod1
            set $terminal foot --title "a terminal"
            set $accent #123456
            set $layout us
            floating_modifier $mod
            input type:keyboard {
                xkb_layout $layout
            }
            client.focused $accent #111111 #ffffff
            bindsym $mod+Return exec $terminal
            bindsym $mod+Shift+q kill
            exec echo "$HOME" $(hostname)
        "#},
        )
        .expect("variable config");
        assert_eq!(config.bindings.len(), 2);
        assert!(config.bindings[0].0.modifiers.alt);
        assert_eq!(
            config.bindings[0].1,
            Action::Exec("foot --title \"a terminal\"".into())
        );
        assert_eq!(
            config.decorations.focused.border,
            Rgba::srgb_u8(0x12, 0x34, 0x56)
        );
        assert_eq!(config.startup[0].command, "echo \"$HOME\" $(hostname)");
        assert!(config.keymap.is_some());
    }

    #[test]
    fn compatibility_skips_whole_unsupported_features_but_keeps_usable_commands() {
        let config: Configuration = parse_with_policy(
            "portable",
            indoc! {r#"
            set $mod Mod4
            blur enable
            bindsym $mod+Return exec foot
            bindsym $mod+s layout stacking
            mode "resize" {
                bindsym Return exec must-not-be-a-global-binding
            }
            input type:touchpad {
                tap enabled
            }
            bar {
                swaybar_command waybar
                mode hide
            }
            bindsym $mod+Tab workspace back_and_forth
            bindsym $mod+Shift+q kill
        "#},
            UnsupportedPolicy::Warn,
            |_| Err(unsupported("unknown command")),
        )
        .expect("portable config");
        assert_eq!(config.bindings.len(), 4);
        assert_eq!(
            config.startup,
            [StartupCommand {
                command: "waybar".into(),
                on_reload: false
            }]
        );
        assert_eq!(
            config
                .warnings
                .iter()
                .map(|warning| warning.line)
                .collect::<Vec<_>>(),
            [2, 13]
        );
        assert_eq!(config.pointer_devices.resolve("pad", true).tap, Some(true));
        assert!(config.pointer.focus_follows_mouse);
        assert!(config.keymap.is_none());
    }

    #[test]
    fn font_descriptions_translate_points_pixels_and_styles() {
        use weld_ssd_config::{
            FontFamily as FontSource, FontSlant as FontStyle, FontWeight, FontWidth,
        };
        let config = parse("font", "set $face monospace\nfont pango:$face 8").expect("font");
        let font = config.decorations.title_font;
        assert_eq!(font.family, FontSource::monospace());
        assert!((font.size - 8.0 * 96.0 / 72.0).abs() < 0.001);
        let config = parse(
            "font",
            "font pango:DejaVu Sans Mono Bold Italic Condensed 18px",
        )
        .expect("font");
        let font = config.decorations.title_font;
        assert_eq!(font.family, FontSource::Family("DejaVu Sans Mono".into()));
        assert_eq!(font.size, 18.0);
        assert_eq!(font.weight, FontWeight::BOLD);
        assert_eq!(font.style, FontStyle::Italic);
        assert_eq!(font.width, FontWidth::CONDENSED);
        assert_eq!(
            parse("font", "font \"serif 12\"")
                .expect("quoted font")
                .decorations
                .title_font
                .family,
            FontSource::serif()
        );
        for source in [
            "font",
            "font monospace",
            "font monospace 0",
            "font monospace 900px",
        ] {
            assert!(parse("font", source).is_err(), "{source}");
        }
    }

    #[test]
    fn named_modes_expand_variables_and_translate_resize_alternatives() {
        let config = parse(
            "modes",
            indoc! {r#"
            set $resize Resize windows
            bindsym Mod4+r mode "$resize"
            bindsym h focus left
            mode --pango_markup "$resize" {
                bindsym h resize shrink height 10 px or 5 ppt
                bindsym Return mode "default"
                bindsym Escape mode "default"
            }
        "#},
        )
        .expect("modes");
        assert_eq!(config.bindings.len(), 2);
        assert_eq!(config.modes.len(), 1);
        let mode = &config.modes[0];
        assert_eq!(mode.name, "Resize windows");
        assert!(mode.pango_markup);
        assert_eq!(
            mode.bindings[0].1,
            Action::Resize(weld_i3_config::I3ResizeRequest {
                axis: SplitAxis::Vertical,
                pixels: Some(-10.0),
                fraction: Some(-0.05),
            })
        );
        for invalid in [
            "bindsym r mode missing",
            "mode resize {\n bindsym r mode missing\n}",
            "mode resize {\n bindsym h focus left\n bindsym h focus right\n}",
            "bindsym h resize grow width 10 px or 20 px",
        ] {
            assert!(parse("invalid", invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn compatibility_keeps_malformed_supported_values_and_missing_variables_fatal() {
        for source in [
            "set $mod",
            "floating_modifier $missing",
            "bindsym $missing+q kill",
            "gaps inner typo",
            "bindsym Mod1+q floating typo",
            "default_orientation diagonal",
            "include missing.conf",
        ] {
            let config: Result<Configuration> =
                parse_with_policy("invalid", source, UnsupportedPolicy::Warn, |_| {
                    Err(unsupported("unknown command"))
                });
            assert!(config.is_err(), "{source}");
        }
    }

    #[test]
    fn valid_unimplemented_sway_forms_warn_without_discarding_supported_bindings() {
        for setting in [
            "default_orientation auto",
            "focus_follows_mouse always",
            "gaps horizontal 5",
            "smart_gaps inverse_outer",
            "smart_borders no_gaps",
            "workspace 1",
            "bar bar-0 {\n swaybar_command should-not-start\n}",
            "bindsym Mod4+x move scratchpad",
            "bindsym Mod4+x focus output right",
            "bindsym Mod4+x focus tiling",
            "bindsym Mod4+x fullscreen toggle global",
            "bindsym Mod4+x resize shrink width 10px",
            "bindsym --to-code Mod4+x exec foot",
        ] {
            let source = format!(
                "{setting}\nbindsym Mod4+Return exec foot\nfloating_modifier Mod4 normal\n"
            );
            let config: Configuration =
                parse_with_policy("portable", &source, UnsupportedPolicy::Warn, |_| {
                    Err(unsupported("unknown"))
                })
                .expect(setting);
            assert_eq!(config.bindings.len(), 1, "{setting}");
            assert_eq!(config.warnings.len(), 1, "{setting}");
            assert!(config.startup.is_empty(), "{setting}");
            assert!(config.pointer.modifier.expect("normal modifier").super_key);
            if setting.contains("--to-code") {
                assert!(
                    config.warnings[0]
                        .message
                        .contains("unsupported bindsym flag --to-code")
                );
            }
        }
    }
    #[test]
    fn workspace_commands_preserve_names_and_validate_selectors() {
        let quoted = parse(
            "quoted",
            r#"bindsym Mod1+1 workspace "1: work, play; rest""#,
        )
        .expect("quoted punctuation");
        assert_eq!(
            quoted.bindings[0].1,
            Action::Workspace(I3WorkspaceRequest::switch(WorkspaceTarget::Name(
                "1: work, play; rest".to_owned()
            )))
        );
        let config = parse(
            "workspaces",
            indoc! {r#"
            workspace "3: work" output missing DP-1
            workspace "3: work" output eDP-1
            bindsym Mod1+3 workspace number "3: work"
            bindsym Mod1+Shift+3 move container to workspace number 3
            bindsym Mod1+4 workspace "next"
            bindsym Mod1+5 workspace next_on_output
        "#},
        )
        .expect("workspace syntax");
        assert_eq!(
            config.workspaces.assignments,
            [WorkspaceAssignment {
                workspace: "3: work".to_owned(),
                outputs: vec!["missing".to_owned(), "DP-1".to_owned()]
            }]
        );
        assert_eq!(
            config.bindings[0].1,
            Action::Workspace(I3WorkspaceRequest::switch(WorkspaceTarget::Number(
                "3: work".to_owned()
            )))
        );
        assert_eq!(
            config.bindings[1].1,
            Action::Workspace(I3WorkspaceRequest::move_window(WorkspaceTarget::Number(
                "3".to_owned()
            )))
        );
        assert_eq!(
            config.bindings[2].1,
            Action::Workspace(I3WorkspaceRequest::switch(WorkspaceTarget::Name(
                "next".to_owned()
            )))
        );
        assert_eq!(
            config.bindings[3].1,
            Action::Workspace(I3WorkspaceRequest::switch(WorkspaceTarget::NextOnOutput))
        );
        for source in [
            "workspace 1 output",
            "workspace \"\" output DP-1",
            "bindsym Mod1+1 workspace",
            "bindsym Mod1+1 workspace number nope",
            "bindsym Mod1+1 workspace number 2147483648",
            "bindsym Mod1+1 workspace $unexpanded",
            "bindsym Mod1+1 workspace 1; exec foot",
        ] {
            assert!(parse("invalid", source).is_err(), "{source}");
        }
    }
    #[test]
    fn focus_wrapping_is_validated_as_behavior_settings() {
        for (value, expected) in [
            ("no", FocusWrapping::No),
            ("yes", FocusWrapping::Yes),
            ("force", FocusWrapping::Force),
            ("workspace", FocusWrapping::Workspace),
        ] {
            let config = parse("test", &format!("focus_wrapping {value}")).expect("wrapping");
            assert_eq!(config.focus_wrapping, expected);
        }
        assert!(parse("test", "focus_wrapping perhaps").is_err());
    }

    #[test]
    fn hover_focus_is_an_explicit_live_pointer_setting() {
        assert!(
            parse("test", "focus_follows_mouse yes")
                .expect("enabled")
                .pointer
                .focus_follows_mouse
        );
        assert!(
            !parse("test", "focus_follows_mouse no")
                .expect("disabled")
                .pointer
                .focus_follows_mouse
        );
        assert!(parse("test", "focus_follows_mouse perhaps").is_err());
    }

    #[test]
    fn exec_preserves_shell_quoting_without_running_during_parse() {
        let config =
            parse("test", "bindsym Mod4+Return exec foot --title \"a b\"").expect("exec config");
        assert_eq!(
            config.bindings[0].1,
            Action::Exec("foot --title \"a b\"".to_owned())
        );
    }

    #[test]
    fn startup_commands_keep_order_reload_policy_and_shell_syntax() {
        let config = parse(
            "startup",
            indoc! {r#"
            exec --no-startup-id waybar --config "a b.json"
            exec_always "printf '%s' "$HOME""
            exec 'foot --title terminal'
            bindsym Mod1+space exec --no-startup-id rofi -show drun
        "#},
        )
        .expect("startup commands");
        assert_eq!(
            config.startup,
            [
                StartupCommand {
                    command: "waybar --config \"a b.json\"".to_owned(),
                    on_reload: false
                },
                StartupCommand {
                    command: "printf '%s' \"$HOME\"".to_owned(),
                    on_reload: true
                },
                StartupCommand {
                    command: "foot --title terminal".to_owned(),
                    on_reload: false
                },
            ]
        );
        assert_eq!(
            config.bindings[0].1,
            Action::Exec("rofi -show drun".to_owned())
        );
        for command in ["exec", "exec_always --no-startup-id", "exec \"\""] {
            let error =
                parse("startup", &format!("# comment\n{command}")).expect_err("missing command");
            assert!(format!("{error:#}").contains("startup:2:"));
        }
    }
    #[test]
    fn settings_and_commands_translate_without_exposing_sway_to_the_tiler() {
        let config = parse(
            "example",
            indoc! {"
            gaps inner 12
            default_orientation vertical
            bindsym Mod4+Left focus left
            bindsym Mod4+Control+Right resize grow width 5 ppt
            bindsym Mod4+Shift+i weld persistent toggle
        "},
        )
        .expect("supported subset");
        assert_eq!(config.tiling.inner_gap, 12);
        assert_eq!(config.tiling.default_axis, SplitAxis::Vertical);
        assert_eq!(config.bindings[0].1, Action::Focus(Direction::Left));
        assert_eq!(
            config.bindings[2].1,
            Action::Persistent(PersistenceAction::Toggle(PersistenceMatch::Auto))
        );
        assert_eq!(
            config.bindings[1].1,
            Action::Resize(weld_i3_config::I3ResizeRequest {
                axis: SplitAxis::Horizontal,
                pixels: None,
                fraction: Some(0.05)
            })
        );
    }
    #[test]
    fn unsupported_or_ambiguous_configuration_is_not_silently_ignored() {
        for source in [
            "include other.conf",
            "gaps inner -1",
            "gaps inner 65536",
            "bindsym Mod4+unknown focus left",
            indoc! {"
                bindsym Mod4+Left focus left
                bindsym Mod4+Left focus right
            "},
            "bindsym Mod4+Left weld persistent enable missing_id",
        ] {
            assert!(parse("example", source).is_err(), "{source}");
        }
        let error = parse(
            "example",
            indoc! {"
            # comment
            gaps inner nope
        "},
        )
        .expect_err("bad number");
        assert!(format!("{error:#}").contains("example:2:"));
    }
    #[test]
    fn floating_configuration_uses_live_modifiers_and_explicit_commands() {
        let config = parse(
            "float",
            indoc! {"
        floating_modifier Mod1+Control
        bindsym Mod1+space floating toggle
        bindsym Mod1+p focus mode_toggle
    "},
        )
        .expect("config");
        assert_eq!(
            config.pointer.modifier,
            Some(weld_input_config::PointerShortcutModifiers {
                alt: true,
                control: true,
                ..Default::default()
            })
        );
        assert_eq!(config.bindings[0].1, Action::Floating(None));
        assert_eq!(config.bindings[1].1, Action::FocusModeToggle);
        assert!(parse("float", "floating_modifier Mod1+Mod1").is_err());
    }

    #[test]
    fn hierarchy_and_sticky_bindings_compile_with_variables() {
        let config = parse(
            "selection",
            indoc! {"
            set $mod Mod4
            bindsym $mod+Control+p focus mode_toggle
            bindsym $mod+Shift+p sticky toggle
            bindsym $mod+p focus parent
            bindsym $mod+Control+Shift+p focus child
        "},
        )
        .expect("config");
        let actions: Vec<_> = config
            .bindings
            .into_iter()
            .map(|(_, action)| action)
            .collect();
        assert_eq!(
            actions,
            [
                Action::FocusModeToggle,
                Action::Sticky(None),
                Action::FocusHierarchy(weld_i3_config::I3FocusHierarchy::Parent),
                Action::FocusHierarchy(weld_i3_config::I3FocusHierarchy::Child)
            ]
        );
        assert!(parse("bad", "bindsym Mod4+p sticky maybe").is_err());
    }

    #[test]
    fn smart_spacing_translates_to_independent_native_policies() {
        let enabled = parse(
            "smart",
            indoc::indoc! {"
            smart_gaps on
            smart_borders on
        "},
        )
        .expect("config");
        assert!(enabled.tiling.hide_solo_gaps);
        assert!(enabled.decorations.hide_solo_border);
        let disabled = parse("smart", "smart_gaps off\nsmart_borders off").expect("config");
        assert!(!disabled.tiling.hide_solo_gaps);
        assert!(!disabled.decorations.hide_solo_border);
        for unsupported in ["smart_gaps maybe", "smart_borders no_gaps"] {
            assert!(parse("unsupported", unsupported).is_err());
        }
    }

    #[test]
    fn fullscreen_commands_translate_to_native_policy() {
        let config = parse(
            "fullscreen",
            indoc::indoc! {"
            bindsym Mod1+a fullscreen
            bindsym Mod1+b fullscreen enable
            bindsym Mod1+c fullscreen disable
        "},
        )
        .expect("config");
        let actions: Vec<_> = config
            .bindings
            .into_iter()
            .map(|(_, action)| action)
            .collect();
        assert_eq!(
            actions,
            vec![
                Action::Fullscreen(FullscreenAction::Toggle(FullscreenMode::Normal)),
                Action::Fullscreen(FullscreenAction::Enable(FullscreenMode::Normal)),
                Action::Fullscreen(FullscreenAction::Disable),
            ]
        );
        assert!(parse("unsupported", "bindsym Mod1+a fullscreen global").is_err());
    }

    #[test]
    fn frame_configuration_validates_dimensions_colors_and_runtime_commands() {
        let config = parse(
            "frames",
            indoc! {"
            default_border pixel 4
            default_floating_border normal 2
            corner_radius 8
            client.focused #112233 #223344 #334455 #445566 #556677
            client.urgent #991122 #882233 #ffffff #ff0000 #ff3344
            bindsym Mod1+b border toggle
            bindsym Mod1+n border none
        "},
        )
        .expect("frames");
        assert_eq!(config.decorations.tiled, BorderStyle::Pixel(4));
        assert_eq!(config.decorations.floating, BorderStyle::Normal(2));
        assert_eq!(
            config.decorations.urgent.child_border,
            Rgba::srgb_u8(0xff, 0x33, 0x44)
        );
        assert_eq!(
            config.decorations.focused.child_border,
            Rgba::srgb_u8(0x55, 0x66, 0x77)
        );
        assert_eq!(config.bindings[0].1, Action::Border(None));
        assert_eq!(
            config.bindings[1].1,
            Action::Border(Some(BorderStyle::None))
        );
        let short =
            parse("frames", "client.focused #112233 #223344 #334455").expect("three colors");
        assert_eq!(
            short.decorations.focused.indicator,
            short.decorations.focused.border
        );
        assert_eq!(
            short.decorations.focused.child_border,
            short.decorations.focused.background
        );
        for bad in [
            "default_border pixel -1",
            "default_border normal 65",
            "default_border none 3",
            "corner_radius 65",
            "client.focused #112233 #223344 #334455 #445566 invalid",
        ] {
            assert!(parse("bad", bad).is_err(), "{bad}");
        }
    }
}
