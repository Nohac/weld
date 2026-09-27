//! Narrow Sway-to-native translation. Unsupported syntax is an error, not an
//! inert setting. This intentionally does not claim a complete Sway interpreter.

use std::collections::HashSet;

use anyhow::{Context, Result, bail, ensure};
use bevy::input::keyboard::KeyCode;
use weld_app::input::{GlobalShortcut, GlobalShortcutModifiers, ShellCommand};
use weld_sway_config::Statement;
use weld_tile::{Direction, SplitAxis, TileOperation, TileSettings};

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Action {
    Tile(TileOperation),
    Reload,
    Shell(ShellCommand),
    Hoist,
    OutputTopology,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Configuration {
    pub tiling: TileSettings,
    pub bindings: Vec<(GlobalShortcut, Action)>,
}

pub(super) fn parse(name: &str, source: &str) -> Result<Configuration> {
    let syntax = weld_sway_config::parse(name, source)?;
    let mut config = Configuration::default();
    let mut chords = HashSet::new();
    for statement in syntax.statements() {
        let offset = statement
            .name()
            .segments()
            .first()
            .map_or(0, |span| span.start);
        let line = source[..offset]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1;
        apply(&mut config, &mut chords, statement)
            .with_context(|| format!("{name}:{line}: {}", statement.header().text()))?;
    }
    Ok(config)
}

fn apply(
    config: &mut Configuration,
    chords: &mut HashSet<GlobalShortcut>,
    statement: &Statement,
) -> Result<()> {
    ensure!(
        statement.block().is_none(),
        "configuration blocks are not supported in this slice"
    );
    let args: Vec<_> = statement
        .arguments()
        .iter()
        .map(|argument| argument.text())
        .collect();
    match (statement.name().text(), args.as_slice()) {
        ("gaps", [kind @ ("inner" | "outer"), value]) => {
            let gap = value
                .parse::<u16>()
                .context("gap must be an integer between 0 and 65535")?;
            if *kind == "inner" {
                config.tiling.inner_gap = gap;
            } else {
                config.tiling.outer_gap = gap;
            }
        }
        ("default_orientation", [value]) => config.tiling.default_axis = axis(value)?,
        ("bindsym", [chord, operation @ ..]) => {
            let chord = binding(chord)?;
            ensure!(chords.insert(chord), "duplicate key binding");
            config.bindings.push((chord, action(operation)?));
        }
        _ => bail!(
            "unsupported Master directive or arguments; see examples/master.sway.config for the current subset"
        ),
    }
    Ok(())
}

fn axis(value: &str) -> Result<SplitAxis> {
    match value {
        "horizontal" => Ok(SplitAxis::Horizontal),
        "vertical" => Ok(SplitAxis::Vertical),
        _ => bail!("orientation must be horizontal or vertical"),
    }
}

fn direction(value: &str) -> Result<Direction> {
    match value {
        "left" => Ok(Direction::Left),
        "right" => Ok(Direction::Right),
        "up" => Ok(Direction::Up),
        "down" => Ok(Direction::Down),
        _ => bail!("direction must be left, right, up or down"),
    }
}

fn action(words: &[&str]) -> Result<Action> {
    Ok(match words {
        ["exec", command @ ..] if !command.is_empty() => {
            // Shell syntax remains unexpanded here. It executes only when this
            // binding is invoked, through the host-owned client launcher.
            Action::Shell(ShellCommand::Launch {
                program: "sh".to_owned(),
                arguments: vec!["-c".to_owned(), command.join(" ")],
            })
        }
        ["exit"] => Action::Shell(ShellCommand::Exit),
        ["weld", "hoist"] => Action::Hoist,
        ["weld", "output-debug"] => Action::OutputTopology,
        ["weld", "scale", "increase"] => Action::Shell(ShellCommand::IncreaseOutputScale),
        ["weld", "scale", "decrease"] => Action::Shell(ShellCommand::DecreaseOutputScale),
        ["weld", "scale", "physical"] => Action::Shell(ShellCommand::MatchOutputPhysicalScale),
        ["splith"] | ["split", "h"] | ["split", "horizontal"] => {
            Action::Tile(TileOperation::Split(SplitAxis::Horizontal))
        }
        ["splitv"] | ["split", "v"] | ["split", "vertical"] => {
            Action::Tile(TileOperation::Split(SplitAxis::Vertical))
        }
        ["focus", value] => Action::Tile(TileOperation::Focus(direction(value)?)),
        ["move", value] => Action::Tile(TileOperation::Move(direction(value)?)),
        [
            "resize",
            kind @ ("grow" | "shrink"),
            dimension,
            value,
            "ppt",
        ] => {
            let percent = value
                .parse::<u8>()
                .context("resize percentage must be an integer")?;
            ensure!(
                (1..=100).contains(&percent),
                "resize percentage must be between 1 and 100"
            );
            let axis = match *dimension {
                "width" => SplitAxis::Horizontal,
                "height" => SplitAxis::Vertical,
                _ => bail!("resize dimension must be width or height"),
            };
            Action::Tile(TileOperation::Resize {
                axis,
                fraction: f32::from(percent) / 100.0 * if *kind == "grow" { 1.0 } else { -1.0 },
            })
        }
        ["kill"] => Action::Tile(TileOperation::Close),
        ["reload"] => Action::Reload,
        _ => bail!("unsupported Master command"),
    })
}

fn binding(value: &str) -> Result<GlobalShortcut> {
    let mut parts = value.split('+').collect::<Vec<_>>();
    let key = parts.pop().context("missing binding key")?;
    let mut modifiers = GlobalShortcutModifiers::default();
    for part in parts {
        let enabled = match part {
            "Mod4" => &mut modifiers.super_key,
            "Mod1" => &mut modifiers.alt,
            "Control" | "Ctrl" => &mut modifiers.control,
            "Shift" => &mut modifiers.shift,
            _ => bail!("unsupported modifier {part}; variables are not implemented yet"),
        };
        ensure!(!*enabled, "duplicate binding modifier");
        *enabled = true;
    }
    // This bootstrap maps key names to physical positions, just like the
    // existing shortcut API. It is not yet XKB-aware bindsym compatibility.
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
    fn default_navigation_uses_letters_without_a_launch_conflict() {
        let config =
            parse("default", include_str!("../../examples/master.sway.config")).expect("config");
        for (key, direction) in [
            (KeyCode::KeyD, Direction::Left),
            (KeyCode::KeyF, Direction::Right),
            (KeyCode::KeyK, Direction::Up),
            (KeyCode::KeyJ, Direction::Down),
        ] {
            for (modifiers, operation) in [
                (
                    GlobalShortcutModifiers::super_key(),
                    TileOperation::Focus(direction),
                ),
                (
                    GlobalShortcutModifiers::super_shift(),
                    TileOperation::Move(direction),
                ),
            ] {
                let binding = config
                    .bindings
                    .iter()
                    .find(|(chord, _)| *chord == GlobalShortcut::new(key, modifiers))
                    .expect("navigation");
                assert_eq!(binding.1, Action::Tile(operation));
            }
        }
        assert_eq!(
            action(&["weld", "hoist"]).expect("extension"),
            Action::Hoist
        );
        assert!(action(&["weld", "persistent", "toggle"]).is_err());
    }

    #[test]
    fn exec_preserves_shell_quoting_without_running_during_parse() {
        let config =
            parse("test", "bindsym Mod4+Return exec foot --title \"a b\"").expect("exec config");
        assert_eq!(
            config.bindings[0].1,
            Action::Shell(ShellCommand::Launch {
                program: "sh".to_owned(),
                arguments: vec!["-c".to_owned(), "foot --title \"a b\"".to_owned()],
            })
        );
    }
    #[test]
    fn settings_and_commands_translate_without_exposing_sway_to_the_tiler() {
        let config = parse("example", "gaps inner 12\ndefault_orientation vertical\nbindsym Mod4+Left focus left\nbindsym Mod4+Control+Right resize grow width 5 ppt").expect("supported subset");
        assert_eq!(config.tiling.inner_gap, 12);
        assert_eq!(config.tiling.default_axis, SplitAxis::Vertical);
        assert_eq!(
            config.bindings[0].1,
            Action::Tile(TileOperation::Focus(Direction::Left))
        );
        assert_eq!(
            config.bindings[1].1,
            Action::Tile(TileOperation::Resize {
                axis: SplitAxis::Horizontal,
                fraction: 0.05
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
            "bindsym Mod4+Left focus left\nbindsym Mod4+Left focus right",
            "bindsym Mod4+Left weld persistent toggle",
        ] {
            assert!(parse("example", source).is_err(), "{source}");
        }
        let error = parse("example", "# comment\ngaps inner nope").expect_err("bad number");
        assert!(format!("{error:#}").contains("example:2:"));
    }
}
