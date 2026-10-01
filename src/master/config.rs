//! Distribution commands layered onto the i3 configuration interpreter.

use anyhow::{Result, bail};
use weld_app::input::ShellCommand;
use weld_i3_quirks::config as i3;

pub(super) type Action = i3::Action<DistributionAction>;
pub(super) type Configuration = i3::Configuration<DistributionAction>;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum DistributionAction {
    Hoist,
    OutputTopology,
    Shell(ShellCommand),
}

pub(super) fn parse(name: &str, source: &str) -> Result<Configuration> {
    i3::parse_with_extensions(name, source, extension)
}

fn extension(words: &[&str]) -> Result<DistributionAction> {
    Ok(match words {
        ["weld", "hoist"] => DistributionAction::Hoist,
        ["weld", "output-debug"] => DistributionAction::OutputTopology,
        ["weld", "scale", "increase"] => {
            DistributionAction::Shell(ShellCommand::IncreaseOutputScale)
        }
        ["weld", "scale", "decrease"] => {
            DistributionAction::Shell(ShellCommand::DecreaseOutputScale)
        }
        ["weld", "scale", "physical"] => {
            DistributionAction::Shell(ShellCommand::MatchOutputPhysicalScale)
        }
        _ => bail!("unsupported Master command"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_input::{GlobalShortcut, GlobalShortcutModifiers, KeyCode};
    use weld_tile::{Direction, SplitAxis, TileOperation};

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
                    Action::Focus(direction),
                ),
                (
                    GlobalShortcutModifiers {
                        alt: true,
                        shift: true,
                        ..Default::default()
                    },
                    Action::Tile(TileOperation::Move(direction)),
                ),
            ] {
                let binding = config
                    .bindings
                    .iter()
                    .find(|(chord, _)| *chord == GlobalShortcut::new(key, modifiers))
                    .expect("navigation");
                assert_eq!(binding.1, operation);
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
            extension(&["weld", "hoist"]).expect("extension"),
            DistributionAction::Hoist
        );
        assert!(extension(&["weld", "persistent", "toggle"]).is_err());
    }
}
