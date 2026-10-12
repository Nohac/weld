//! Sway keyboard settings and binding chords translated into Weld input types.

mod devices;
mod syntax;

use crate::evaluation::{ConfigWarning, UnsupportedPolicy, line_of, unsupported};
use crate::{ParsedConfig, Statement};
use anyhow::{Context, Result, bail, ensure};
use std::collections::HashSet;
use weld_input::{
    GlobalShortcut, GlobalShortcutModifiers, KeyCode, KeyboardKeymap, KeymapConfig,
    PointerShortcutModifiers,
};
use winnow::Parser;

/// Input configuration plus directives left for other subsystem translators.
#[derive(Debug, Default)]
pub struct InputConfiguration<'a> {
    /// Physical pointer preferences, resolved when the native device is available.
    pub pointer_devices: weld_input::PointerDeviceSettings,
    /// Theme and optional size for compositor-owned cursor artwork.
    pub cursor: Option<weld_input::CursorThemeSettings>,
    /// Compiled keymap; absence restores the native default.
    pub keymap: Option<KeyboardKeymap>,
    /// Native chords paired with commands for the assembly's action translator.
    pub bindings: Vec<Binding>,
    /// Named keyboard contexts, activated by the configuration assembly.
    pub modes: Vec<BindingMode>,
    /// Statements owned by other configuration consumers, in source order.
    pub remaining: Vec<&'a Statement>,
    /// Unsupported input features skipped under the selected policy.
    pub warnings: Vec<ConfigWarning>,
}

/// A native chord and its preserved Sway command spelling.
#[derive(Debug)]
pub struct Binding {
    /// Key and modifier requirements used by Weld's binding registry.
    pub shortcut: GlobalShortcut,
    /// Unexpanded command words, preserving shell quotes for exec.
    pub command: Vec<String>,
    /// One-based physical source line for action diagnostics.
    pub line: usize,
}

#[derive(Debug)]
/// Bindings and display metadata for a named keyboard context.
pub struct BindingMode {
    /// Name referenced by mode-switch commands.
    pub name: String,
    /// Whether bars may interpret the name as Pango markup.
    pub pango_markup: bool,
    /// Chords active while this mode is selected.
    pub bindings: Vec<Binding>,
}

/// Decodes a plain or fully quoted literal argument. Variables, escape sequences
/// and unquoted command separators are rejected until their interpretation is supported.
pub fn literal(value: &str) -> Result<&str> {
    crate::value::parse(value)
}

/// Compile input directives and leave other directives for the assembly.
///
/// Errors retain the source name and directive line. Compilation performs no
/// command execution or application mutation.
pub fn compile<'a>(
    name: &str,
    source: &str,
    syntax: &'a ParsedConfig,
) -> Result<InputConfiguration<'a>> {
    compile_with_policy(name, source, syntax, UnsupportedPolicy::Reject)
}

/// Compile input directives with explicit unsupported-feature handling.
/// Malformed supported settings remain fatal under either policy.
pub fn compile_with_policy<'a>(
    name: &str,
    source: &str,
    syntax: &'a ParsedConfig,
    policy: UnsupportedPolicy,
) -> Result<InputConfiguration<'a>> {
    let mut config = InputConfiguration::default();
    let mut keyboard = KeymapConfig::default();
    let mut keyboard_configured = false;
    let mut chords = HashSet::new();
    for statement in syntax.statements() {
        let line = line_of(source, statement)?;
        let result = match statement.name().text() {
            "input" => apply_devices(&mut config, &mut keyboard, statement, name, source, policy)
                .map(|configured| keyboard_configured |= configured),
            "seat" => devices::apply_seat(&mut config, statement, name, source, policy),
            "bindsym" => (|| {
                config
                    .bindings
                    .push(compile_binding(statement, line, &mut chords)?);
                Ok(())
            })(),
            "mode" if statement.block().is_some() => (|| {
                let words: Vec<_> = statement
                    .arguments()
                    .iter()
                    .map(|word| word.text())
                    .collect();
                let (mode_name, pango_markup) = match words.as_slice() {
                    [value] => (literal(value)?, false),
                    ["--pango_markup", value] => (literal(value)?, true),
                    _ => bail!("mode requires a name and optional --pango_markup flag"),
                };
                ensure!(!mode_name.is_empty(), "mode name must not be empty");
                ensure!(
                    !config.modes.iter().any(|mode| mode.name == mode_name),
                    "duplicate binding mode"
                );
                let mut mode = BindingMode {
                    name: mode_name.into(),
                    pango_markup,
                    bindings: Vec::new(),
                };
                let mut mode_chords = HashSet::new();
                let block = statement.block().context("mode block missing")?;
                for child in block.statements() {
                    let child_line = line_of(source, child)?;
                    let result = if child.name().text() == "bindsym" {
                        compile_binding(
                            child,
                            child_line,
                            if mode_name == "default" {
                                &mut chords
                            } else {
                                &mut mode_chords
                            },
                        )
                    } else {
                        Err(unsupported(format!(
                            "unsupported mode directive {}",
                            child.name().text()
                        )))
                    };
                    match result {
                        Ok(binding) => mode.bindings.push(binding),
                        Err(error) => {
                            policy.handle(error, name, child_line, &mut config.warnings)?
                        }
                    }
                }
                if mode_name == "default" {
                    config.bindings.extend(mode.bindings);
                } else {
                    config.modes.push(mode);
                }
                Ok(())
            })(),
            _ => {
                config.remaining.push(statement);
                Ok(())
            }
        };
        if let Err(error) = result {
            policy.handle(
                error.context(statement.header().text().to_owned()),
                name,
                line,
                &mut config.warnings,
            )?;
        }
    }
    if keyboard_configured {
        config.keymap = Some(
            KeyboardKeymap::compile(&keyboard)
                .with_context(|| format!("{name}: keyboard configuration"))?,
        );
    }
    Ok(config)
}

fn compile_binding(
    statement: &Statement,
    line: usize,
    chords: &mut HashSet<GlobalShortcut>,
) -> Result<Binding> {
    ensure!(
        statement.block().is_none(),
        "bindsym requires a command, not a block"
    );
    let mut words = statement.arguments().iter().map(|word| word.text());
    let chord = words.next().context("missing binding chord")?;
    if chord.starts_with("--") {
        return Err(unsupported(format!(
            "unsupported bindsym flag {chord}; binding skipped"
        )));
    }
    let shortcut = binding(chord)?;
    let command: Vec<_> = words.map(str::to_owned).collect();
    ensure!(!command.is_empty(), "missing binding command");
    ensure!(chords.insert(shortcut), "duplicate key binding");
    Ok(Binding {
        shortcut,
        command,
        line,
    })
}

fn apply_devices(
    config: &mut InputConfiguration,
    keyboard: &mut KeymapConfig,
    statement: &Statement,
    name: &str,
    source: &str,
    policy: UnsupportedPolicy,
) -> Result<bool> {
    let mut args = setting_arguments(statement);
    let selector = literal(args.next().context("missing input selector")?)?;
    let mut keyboard_configured = false;
    if let Some(block) = statement.block() {
        ensure!(args.next().is_none(), "unexpected input block arguments");
        for child in block.statements() {
            ensure!(child.block().is_none(), "nested input configuration block");
            let values: Vec<_> = setting_arguments(child).collect();
            let [value] = values.as_slice() else {
                bail!(
                    "input directive requires one value: {}",
                    child.header().text()
                );
            };
            match devices::apply_field(config, keyboard, selector, child.name().text(), value) {
                Ok(configured) => keyboard_configured |= configured,
                Err(error) => policy.handle(
                    error.context(child.header().text().to_owned()),
                    name,
                    line_of(source, child)?,
                    &mut config.warnings,
                )?,
            }
        }
    } else {
        let words: Vec<_> = args.collect();
        let [field, value] = words.as_slice() else {
            bail!("input requires a block or one directive and value");
        };
        keyboard_configured = devices::apply_field(config, keyboard, selector, field, value)?;
    }
    Ok(keyboard_configured)
}

fn setting_arguments(statement: &Statement) -> impl Iterator<Item = &str> {
    statement
        .arguments()
        .iter()
        .map(|argument| argument.text())
        .take_while(|word| !word.starts_with('#'))
}

fn keyboard_field(config: &mut KeymapConfig, field: &str, value: &str) -> Result<()> {
    let target = match field {
        "xkb_rules" => &mut config.rules,
        "xkb_model" => &mut config.model,
        "xkb_layout" => &mut config.layout,
        "xkb_variant" => &mut config.variant,
        "xkb_options" => &mut config.options,
        _ => {
            return Err(unsupported(format!(
                "unsupported keyboard directive {field}"
            )));
        }
    };
    let value = syntax::xkb_value
        .parse(value)
        .map_err(|error| anyhow::anyhow!("invalid literal XKB value: {error}"))?;
    *target = value.to_owned();
    Ok(())
}

/// Translates the modifier-only chord used by `floating_modifier`.
pub fn pointer_modifiers(value: &str) -> Result<PointerShortcutModifiers> {
    let parts = syntax::modifiers
        .parse(value)
        .map_err(|error| anyhow::anyhow!("invalid modifier chord: {error}"))?;
    let modifiers = resolve_modifiers(parts)?;
    Ok(PointerShortcutModifiers {
        control: modifiers.control,
        alt: modifiers.alt,
        shift: modifiers.shift,
        super_key: modifiers.super_key,
    })
}

fn resolve_modifiers(parts: Vec<syntax::Modifier<'_>>) -> Result<GlobalShortcutModifiers> {
    let mut modifiers = GlobalShortcutModifiers::default();
    for part in parts {
        let enabled = match part.0 {
            "Mod4" => &mut modifiers.super_key,
            "Mod1" => &mut modifiers.alt,
            "Control" | "Ctrl" => &mut modifiers.control,
            "Shift" => &mut modifiers.shift,
            name if name.contains('$') => bail!("unresolved variable in modifier {name}"),
            name => return Err(unsupported(format!("unsupported modifier {name}"))),
        };
        ensure!(!*enabled, "duplicate binding modifier");
        *enabled = true;
    }
    Ok(modifiers)
}

fn binding(value: &str) -> Result<GlobalShortcut> {
    let (mut parts, key) = syntax::chord
        .parse(value)
        .map_err(|error| anyhow::anyhow!("invalid binding chord: {error}"))?;
    if matches!(key, "Mod4" | "Mod1" | "Control" | "Ctrl" | "Shift") {
        parts.push(syntax::Modifier(key));
        return Ok(GlobalShortcut::held(resolve_modifiers(parts)?));
    }
    let modifiers = resolve_modifiers(parts)?;
    // Trigger names currently select physical positions; modifier matching uses
    // the configured XKB state supplied by the native host.
    let key = match key {
        "a" => KeyCode::KeyA,
        "b" => KeyCode::KeyB,
        "c" => KeyCode::KeyC,
        "d" => KeyCode::KeyD,
        "e" => KeyCode::KeyE,
        "f" => KeyCode::KeyF,
        "g" => KeyCode::KeyG,
        "h" => KeyCode::KeyH,
        "i" => KeyCode::KeyI,
        "j" => KeyCode::KeyJ,
        "k" => KeyCode::KeyK,
        "l" => KeyCode::KeyL,
        "m" => KeyCode::KeyM,
        "n" => KeyCode::KeyN,
        "o" => KeyCode::KeyO,
        "p" => KeyCode::KeyP,
        "q" => KeyCode::KeyQ,
        "r" => KeyCode::KeyR,
        "s" => KeyCode::KeyS,
        "t" => KeyCode::KeyT,
        "u" => KeyCode::KeyU,
        "v" => KeyCode::KeyV,
        "w" => KeyCode::KeyW,
        "x" => KeyCode::KeyX,
        "y" => KeyCode::KeyY,
        "z" => KeyCode::KeyZ,
        "0" => KeyCode::Digit0,
        "1" => KeyCode::Digit1,
        "2" => KeyCode::Digit2,
        "3" => KeyCode::Digit3,
        "4" => KeyCode::Digit4,
        "5" => KeyCode::Digit5,
        "6" => KeyCode::Digit6,
        "7" => KeyCode::Digit7,
        "8" => KeyCode::Digit8,
        "9" => KeyCode::Digit9,
        "Return" => KeyCode::Enter,
        "space" => KeyCode::Space,
        "Escape" => KeyCode::Escape,
        "Tab" => KeyCode::Tab,
        "Pause" => KeyCode::Pause,
        "Print" => KeyCode::PrintScreen,
        "XF86AudioRaiseVolume" => KeyCode::AudioVolumeUp,
        "XF86AudioLowerVolume" => KeyCode::AudioVolumeDown,
        "XF86AudioMute" => KeyCode::AudioVolumeMute,
        "XF86AudioPlay" => KeyCode::MediaPlayPause,
        "XF86AudioPause" => weld_input::physical_keycode(weld_input::LinuxKeycode(201)),
        "XF86MonBrightnessDown" => weld_input::physical_keycode(weld_input::LinuxKeycode(224)),
        "XF86MonBrightnessUp" => weld_input::physical_keycode(weld_input::LinuxKeycode(225)),
        "XF86AudioNext" => KeyCode::MediaTrackNext,
        "XF86AudioPrev" => KeyCode::MediaTrackPrevious,
        "equal" => KeyCode::Equal,
        "minus" => KeyCode::Minus,
        "Left" => KeyCode::ArrowLeft,
        "Right" => KeyCode::ArrowRight,
        "Up" => KeyCode::ArrowUp,
        "Down" => KeyCode::ArrowDown,
        "F1" => KeyCode::F1,
        "F2" => KeyCode::F2,
        "F3" => KeyCode::F3,
        "F4" => KeyCode::F4,
        "F5" => KeyCode::F5,
        "F6" => KeyCode::F6,
        "F7" => KeyCode::F7,
        "F8" => KeyCode::F8,
        "F9" => KeyCode::F9,
        "F10" => KeyCode::F10,
        "F11" => KeyCode::F11,
        "F12" => KeyCode::F12,
        key if key.contains('$') => bail!("unresolved variable in key {key}"),
        _ => return Err(unsupported(format!("unsupported key name {key}"))),
    };
    Ok(GlobalShortcut::new(key, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn modifier_meanings_and_aliases_are_validated_after_syntax() {
        assert_eq!(
            binding("Ctrl+f").expect("alias"),
            binding("Control+f").expect("canonical")
        );
        for invalid in ["Hyper+f", "Ctrl+Control+f", "Mod1+f+g"] {
            assert!(binding(invalid).is_err(), "{invalid}");
        }
    }
    fn parse_input(name: &str, source: &str) -> Result<InputConfiguration<'static>> {
        let syntax = crate::parse(name, source)?;
        let config = compile(name, source, &syntax)?;
        assert!(config.remaining.is_empty());
        Ok(InputConfiguration {
            pointer_devices: config.pointer_devices,
            cursor: config.cursor,
            keymap: config.keymap,
            bindings: config.bindings,
            modes: config.modes,
            remaining: Vec::new(),
            warnings: config.warnings,
        })
    }
    #[test]
    fn pointer_and_cursor_directives_compile_without_configuring_a_keyboard() {
        let config = parse_input(
            "devices",
            r#"
            input "type:touchpad" {
                tap enabled
                tap_button_map lrm # Left, Right, Middle
                click_method clickfinger
            }
            input * tap disabled
            seat * {
                xcursor_theme "Example Cursor" 30 # nominal size
            }
        "#,
        )
        .expect("device configuration");
        assert!(config.keymap.is_none());
        let resolved = config.pointer_devices.resolve("pad", true);
        assert_eq!(resolved.tap, Some(true));
        assert_eq!(
            resolved.tap_button_map,
            Some(weld_input::FingerButtonMap::LeftRightMiddle)
        );
        assert_eq!(
            resolved.click_method,
            Some(weld_input::TouchpadClickMethod::Clickfinger)
        );
        let cursor = config.cursor.expect("cursor");
        assert_eq!(cursor.theme, "Example Cursor");
        assert_eq!(cursor.size.map(|size| size.get()), Some(30));
        for invalid in [
            "input type:touchpad tap yes",
            "input * tap_button_map invalid",
            "input * click_method invalid",
            "seat * xcursor_theme Default 0",
        ] {
            let syntax = crate::parse("invalid", invalid).expect("syntax");
            assert!(
                compile_with_policy("invalid", invalid, &syntax, UnsupportedPolicy::Warn).is_err()
            );
        }
    }

    #[test]
    fn keyboard_blocks_and_inline_overrides_compile_into_native_settings() {
        let config = parse_input("keyboard", "input type:keyboard {\n xkb_layout us\n xkb_options \"ctrl:swapcaps\"\n}\ninput * xkb_options \"ctrl:swapcaps,altwin:swap_lalt_lwin\"").expect("input config");
        let mut mapper = weld_input::KeyboardMapper::new(config.keymap.expect("configured map"))
            .expect("mapper");
        for (code, expected) in [
            (
                58,
                weld_input::SeatModifiers {
                    control: true,
                    ..Default::default()
                },
            ),
            (
                125,
                weld_input::SeatModifiers {
                    control: true,
                    alt: true,
                    ..Default::default()
                },
            ),
        ] {
            let event = mapper.resolve(weld_input::RawSeatEvent::new(
                weld_input::RawSeatEventKind::Keyboard {
                    keycode: weld_input::LinuxKeycode(code),
                    logical_key: None,
                    state: weld_input::KeyboardKeyState::Pressed,
                },
                0,
            ));
            assert_eq!(event.modifiers, Some(expected));
        }
        for source in [
            "input type:keyboard xkb_layout nonexistent_weld_layout",
            "input type:keyboard xkb_layout \"us\0de\"",
            "input type:keyboard xkb_layout $layout",
            "input type:mouse xkb_layout us",
            "input type:keyboard repeat_rate 40",
        ] {
            assert!(parse_input("keyboard", source).is_err(), "{source}");
        }
        assert!(
            parse_input("reset", "")
                .expect("default config")
                .keymap
                .is_none()
        );
    }
}
