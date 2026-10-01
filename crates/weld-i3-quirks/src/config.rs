//! Sway configuration interpretation for the i3 behavior assembly.

use crate::FocusWrapping;
use anyhow::{Context, Result, bail, ensure};
use weld_input::{GlobalShortcut, KeyboardKeymap};
use weld_sway_config::Statement;
use weld_tile::{Direction, SplitAxis, TileOperation, TileSettings};

#[derive(Clone, Debug, PartialEq)]
pub enum Action<Extension = ()> {
    Focus(Direction),
    Move(Direction),
    Tile(TileOperation),
    Reload,
    Exec(String),
    Exit,
    Extension(Extension),
}

#[derive(Clone, Debug)]
pub struct Configuration<Extension = ()> {
    pub tiling: TileSettings,
    pub focus_wrapping: FocusWrapping,
    pub bindings: Vec<(GlobalShortcut, Action<Extension>)>,
    pub keymap: Option<KeyboardKeymap>,
}

impl<Extension> Default for Configuration<Extension> {
    fn default() -> Self {
        Self {
            tiling: TileSettings::default(),
            focus_wrapping: FocusWrapping::default(),
            bindings: Vec::new(),
            keymap: None,
        }
    }
}

pub fn parse(name: &str, source: &str) -> Result<Configuration> {
    parse_with_extensions(name, source, |_| bail!("unsupported i3 command"))
}

/// Interprets i3 settings and commands, asking the distribution to validate
/// additional command vocabulary. Errors retain the binding's source location.
pub fn parse_with_extensions<Extension>(
    name: &str,
    source: &str,
    extension: impl Fn(&[&str]) -> Result<Extension>,
) -> Result<Configuration<Extension>> {
    let syntax = weld_sway_config::parse(name, source)?;
    let input = weld_sway_config::input::compile(name, source, &syntax)?;
    let mut config = Configuration {
        keymap: input.keymap,
        ..Default::default()
    };
    for binding in input.bindings {
        let words: Vec<_> = binding.command.iter().map(String::as_str).collect();
        let action = action(&words, &extension)
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

fn apply<Extension>(config: &mut Configuration<Extension>, statement: &Statement) -> Result<()> {
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
        ("focus_wrapping", [value]) => {
            config.focus_wrapping = match *value {
                "no" => FocusWrapping::No,
                "yes" => FocusWrapping::Yes,
                "force" => FocusWrapping::Force,
                "workspace" => FocusWrapping::Workspace,
                _ => bail!("focus_wrapping must be no, yes, force or workspace"),
            };
        }
        _ => bail!("unsupported i3 configuration directive or arguments"),
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

fn action<Extension>(
    words: &[&str],
    extension: &impl Fn(&[&str]) -> Result<Extension>,
) -> Result<Action<Extension>> {
    Ok(match words {
        ["exec", command @ ..] if !command.is_empty() => {
            // Shell syntax remains unexpanded here. It executes only when this
            // binding is invoked, through the host-owned client launcher.
            Action::Exec(command.join(" "))
        }
        ["exit"] => Action::Exit,
        ["splith"] | ["split", "h"] | ["split", "horizontal"] => {
            Action::Tile(TileOperation::Split(SplitAxis::Horizontal))
        }
        ["splitv"] | ["split", "v"] | ["split", "vertical"] => {
            Action::Tile(TileOperation::Split(SplitAxis::Vertical))
        }
        ["focus", value] => Action::Focus(direction(value)?),
        ["move", value] => Action::Move(direction(value)?),
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
        _ => Action::Extension(extension(words)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn exec_preserves_shell_quoting_without_running_during_parse() {
        let config =
            parse("test", "bindsym Mod4+Return exec foot --title \"a b\"").expect("exec config");
        assert_eq!(
            config.bindings[0].1,
            Action::Exec("foot --title \"a b\"".to_owned())
        );
    }
    #[test]
    fn settings_and_commands_translate_without_exposing_sway_to_the_tiler() {
        let config = parse("example", "gaps inner 12\ndefault_orientation vertical\nbindsym Mod4+Left focus left\nbindsym Mod4+Control+Right resize grow width 5 ppt").expect("supported subset");
        assert_eq!(config.tiling.inner_gap, 12);
        assert_eq!(config.tiling.default_axis, SplitAxis::Vertical);
        assert_eq!(config.bindings[0].1, Action::Focus(Direction::Left));
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
