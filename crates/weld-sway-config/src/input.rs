//! Sway keyboard settings and binding chords translated into Weld input types.

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
    /// Compiled keymap; absence restores the native default.
    pub keymap: Option<KeyboardKeymap>,
    /// Native chords paired with commands for the assembly's action translator.
    pub bindings: Vec<Binding>,
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
            "input" => apply_keyboard(
                &mut keyboard,
                statement,
                name,
                source,
                policy,
                &mut config.warnings,
            )
            .map(|()| keyboard_configured = true),
            "bindsym" => (|| {
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
                config.bindings.push(Binding {
                    shortcut,
                    command,
                    line,
                });
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

fn apply_keyboard(
    config: &mut KeymapConfig,
    statement: &Statement,
    name: &str,
    source: &str,
    policy: UnsupportedPolicy,
    warnings: &mut Vec<ConfigWarning>,
) -> Result<()> {
    let mut args = statement.arguments().iter().map(|arg| arg.text());
    let selector = literal(args.next().context("missing input selector")?)?;
    if !matches!(selector, "type:keyboard" | "*") {
        return Err(unsupported(
            "only type:keyboard and * keyboard input settings are supported",
        ));
    }
    if let Some(block) = statement.block() {
        ensure!(args.next().is_none(), "unexpected input block arguments");
        for child in block.statements() {
            ensure!(
                child.block().is_none(),
                "nested keyboard configuration block"
            );
            let values: Vec<_> = child.arguments().iter().map(|arg| arg.text()).collect();
            let [value] = values.as_slice() else {
                bail!(
                    "keyboard directive requires one value: {}",
                    child.header().text()
                );
            };
            if let Err(error) = keyboard_field(config, child.name().text(), value) {
                policy.handle(
                    error.context(child.header().text().to_owned()),
                    name,
                    line_of(source, child)?,
                    warnings,
                )?;
            }
        }
    } else {
        let words: Vec<_> = args.collect();
        let [field, value] = words.as_slice() else {
            bail!("input requires a block or one keyboard directive and value");
        };
        keyboard_field(config, field, value)?;
    }
    Ok(())
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
    let (parts, key) = syntax::chord
        .parse(value)
        .map_err(|error| anyhow::anyhow!("invalid binding chord: {error}"))?;
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
            keymap: config.keymap,
            bindings: config.bindings,
            remaining: Vec::new(),
            warnings: config.warnings,
        })
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
