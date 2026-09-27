//! Narrow Sway-to-native translation. Unsupported syntax is an error, not an
//! inert setting. This intentionally does not claim a complete Sway interpreter.

use anyhow::{Context, Result, bail, ensure};
use weld_app::input::ShellCommand;
use weld_input::{GlobalShortcut, KeyboardKeymap};
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
    pub keymap: Option<KeyboardKeymap>,
}

pub(super) fn parse(name: &str, source: &str) -> Result<Configuration> {
    let syntax = weld_sway_config::parse(name, source)?;
    let input = weld_sway_config::input::compile(name, source, &syntax)?;
    let mut config = Configuration {
        keymap: input.keymap,
        ..Default::default()
    };
    for binding in input.bindings {
        let words: Vec<_> = binding.command.iter().map(String::as_str).collect();
        let action = action(&words)
            .with_context(|| format!("{name}:{}: {}", binding.line, binding.command.join(" ")))?;
        config.bindings.push((binding.shortcut, action));
    }
    for statement in input.remaining {
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
        apply(&mut config, statement)
            .with_context(|| format!("{name}:{line}: {}", statement.header().text()))?;
    }
    Ok(config)
}

fn apply(config: &mut Configuration, statement: &Statement) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use weld_input::{GlobalShortcutModifiers, KeyCode};
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
                    GlobalShortcutModifiers {
                        alt: true,
                        ..Default::default()
                    },
                    TileOperation::Focus(direction),
                ),
                (
                    GlobalShortcutModifiers {
                        alt: true,
                        shift: true,
                        ..Default::default()
                    },
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
        let modifier = GlobalShortcutModifiers {
            alt: true,
            control: true,
            ..Default::default()
        };
        for (key, axis) in [
            (KeyCode::KeyF, SplitAxis::Horizontal),
            (KeyCode::KeyJ, SplitAxis::Vertical),
        ] {
            assert!(config.bindings.contains(&(
                GlobalShortcut::new(key, modifier),
                Action::Tile(TileOperation::Split(axis))
            )));
        }
        assert!(config.bindings.contains(&(
            GlobalShortcut::new(
                KeyCode::KeyQ,
                GlobalShortcutModifiers {
                    alt: true,
                    shift: true,
                    ..Default::default()
                }
            ),
            Action::Tile(TileOperation::Close)
        )));
        assert!(!config.bindings.iter().any(|(chord, _)| matches!(
            chord.trigger,
            KeyCode::F1
                | KeyCode::F2
                | KeyCode::F3
                | KeyCode::F4
                | KeyCode::F5
                | KeyCode::F6
                | KeyCode::F7
                | KeyCode::F8
                | KeyCode::F9
                | KeyCode::F10
                | KeyCode::F11
                | KeyCode::F12
        )));
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
