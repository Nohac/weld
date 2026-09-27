//! Sway keyboard settings and binding chords translated into Weld input types.

mod syntax;

use crate::{ParsedConfig, Statement};
use anyhow::{Context, Result, bail, ensure};
use std::collections::HashSet;
use weld_input::{GlobalShortcut, GlobalShortcutModifiers, KeyCode, KeyboardKeymap, KeymapConfig};
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

/// Compile input directives and leave other directives for the assembly.
///
/// Errors retain the source name and directive line. Compilation performs no
/// command execution or application mutation.
pub fn compile<'a>(
    name: &str,
    source: &str,
    syntax: &'a ParsedConfig,
) -> Result<InputConfiguration<'a>> {
    let mut config = InputConfiguration::default();
    let mut keyboard = KeymapConfig::default();
    let mut keyboard_configured = false;
    let mut chords = HashSet::new();
    for statement in syntax.statements() {
        let offset = statement
            .name()
            .segments()
            .first()
            .map_or(0, |span| span.start);
        let line = source
            .get(..offset)
            .context("syntax offset is outside its source")?
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1;
        let result = match statement.name().text() {
            "input" => {
                keyboard_configured = true;
                apply_keyboard(&mut keyboard, statement)
            }
            "bindsym" => (|| {
                ensure!(
                    statement.block().is_none(),
                    "bindsym requires a command, not a block"
                );
                let mut words = statement.arguments().iter().map(|word| word.text());
                let shortcut = binding(words.next().context("missing binding chord")?)?;
                ensure!(chords.insert(shortcut), "duplicate key binding");
                let command: Vec<_> = words.map(str::to_owned).collect();
                ensure!(!command.is_empty(), "missing binding command");
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
        result.with_context(|| format!("{name}:{line}: {}", statement.header().text()))?;
    }
    if keyboard_configured {
        config.keymap = Some(
            KeyboardKeymap::compile(&keyboard)
                .with_context(|| format!("{name}: keyboard configuration"))?,
        );
    }
    Ok(config)
}

fn apply_keyboard(config: &mut KeymapConfig, statement: &Statement) -> Result<()> {
    let mut args = statement.arguments().iter().map(|arg| arg.text());
    ensure!(
        matches!(args.next(), Some("type:keyboard" | "*")),
        "keyboard input selector must be type:keyboard or *; per-device settings are not supported yet"
    );
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
            keyboard_field(config, child.name().text(), value)
                .with_context(|| child.header().text().to_owned())?;
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
        _ => bail!("unsupported keyboard directive {field}"),
    };
    let value = syntax::xkb_value
        .parse(value)
        .map_err(|error| anyhow::anyhow!("invalid literal XKB value: {error}"))?;
    *target = value.to_owned();
    Ok(())
}

fn binding(value: &str) -> Result<GlobalShortcut> {
    let (parts, key) = syntax::chord
        .parse(value)
        .map_err(|error| anyhow::anyhow!("invalid binding chord: {error}"))?;
    let mut modifiers = GlobalShortcutModifiers::default();
    for part in parts {
        let enabled = match part.0 {
            "Mod4" => &mut modifiers.super_key,
            "Mod1" => &mut modifiers.alt,
            "Control" | "Ctrl" => &mut modifiers.control,
            "Shift" => &mut modifiers.shift,
            name => bail!("unsupported modifier {name}"),
        };
        ensure!(!*enabled, "duplicate binding modifier");
        *enabled = true;
    }
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
        "Return" => KeyCode::Enter,
        "Escape" => KeyCode::Escape,
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
        _ => bail!("unsupported key name; see the documented physical-key subset"),
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
