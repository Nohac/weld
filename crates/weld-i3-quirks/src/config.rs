//! Sway configuration interpretation for the i3 behavior assembly.

use crate::FocusWrapping;
use crate::workspace::{
    I3WorkspaceRequest, WorkspaceAssignment, WorkspaceSettings, WorkspaceTarget, number,
};
use anyhow::{Context, Result, bail, ensure};
use weld_input::{GlobalShortcut, KeyboardKeymap};
use weld_sway_config::Statement;
use weld_tile::{Direction, SplitAxis, TileOperation, TileSettings};

#[derive(Clone, Debug, PartialEq)]
pub enum Action<Extension = ()> {
    Focus(Direction),
    Move(Direction),
    Workspace(I3WorkspaceRequest),
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
    pub workspaces: WorkspaceSettings,
    pub startup: Vec<StartupCommand>,
}

/// A validated shell command and its configuration-load execution policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupCommand {
    pub command: String,
    pub on_reload: bool,
}

impl<Extension> Default for Configuration<Extension> {
    fn default() -> Self {
        Self {
            tiling: TileSettings::default(),
            focus_wrapping: FocusWrapping::default(),
            bindings: Vec::new(),
            keymap: None,
            workspaces: WorkspaceSettings::default(),
            startup: Vec::new(),
        }
    }
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
        if let Action::Workspace(I3WorkspaceRequest::Switch(
            WorkspaceTarget::Name(name) | WorkspaceTarget::Number(name),
        )) = &action
            && !config.workspaces.initial_names.contains(name)
        {
            config.workspaces.initial_names.push(name.clone());
        }
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
        (directive @ ("exec" | "exec_always"), command) => {
            config.startup.push(StartupCommand {
                command: exec_command(command)?,
                on_reload: directive == "exec_always",
            });
        }
        ("workspace", [name, "output", outputs @ ..]) if !outputs.is_empty() => {
            let name = weld_sway_config::input::literal(name)?;
            ensure!(!name.is_empty(), "workspace name must not be empty");
            // i3 keeps the first assignment directive for each exact name.
            if config
                .workspaces
                .assignments
                .iter()
                .any(|assignment| assignment.workspace == name)
            {
                return Ok(());
            }
            let outputs = outputs
                .iter()
                .map(|value| weld_sway_config::input::literal(value).map(str::to_owned))
                .collect::<Result<Vec<_>>>()?;
            ensure!(
                outputs.iter().all(|output| !output.is_empty()),
                "output name must not be empty"
            );
            config.workspaces.assignments.push(WorkspaceAssignment {
                workspace: name.to_owned(),
                outputs,
            });
        }
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
        ["workspace", target @ ..] => {
            Action::Workspace(I3WorkspaceRequest::Switch(workspace_target(target)?))
        }
        [
            "move",
            "container" | "window",
            "to",
            "workspace",
            target @ ..,
        ]
        | ["move", "to", "workspace", target @ ..]
        | ["move", "workspace", target @ ..] => {
            Action::Workspace(I3WorkspaceRequest::MoveWindow(workspace_target(target)?))
        }
        ["exec" | "exec_always", command @ ..] => {
            // Shell syntax remains unexpanded here. It executes only when this
            // binding is invoked, through the host-owned client launcher.
            Action::Exec(exec_command(command)?)
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

fn exec_command(words: &[&str]) -> Result<String> {
    let words = words.strip_prefix(&["--no-startup-id"]).unwrap_or(words);
    ensure!(!words.is_empty(), "exec requires a shell command");
    // Sway permits quoting the complete shell command as one config argument.
    if let [word] = words {
        for quote in ['\'', '"'] {
            if let Some(command) = word
                .strip_prefix(quote)
                .and_then(|word| word.strip_suffix(quote))
            {
                ensure!(!command.trim().is_empty(), "exec requires a shell command");
                return Ok(command.to_owned());
            }
        }
    }
    Ok(words.join(" "))
}

fn workspace_target(words: &[&str]) -> Result<WorkspaceTarget> {
    if let ["number", name @ ..] = words {
        let name = workspace_name(name)?;
        ensure!(
            number(&name).is_some(),
            "workspace number requires a leading nonnegative integer"
        );
        return Ok(WorkspaceTarget::Number(name));
    }
    Ok(match words {
        ["next"] => WorkspaceTarget::Next,
        ["prev"] => WorkspaceTarget::Previous,
        ["next_on_output"] => WorkspaceTarget::NextOnOutput,
        ["prev_on_output"] => WorkspaceTarget::PreviousOnOutput,
        ["back_and_forth"] => WorkspaceTarget::BackAndForth,
        ["current"] => WorkspaceTarget::Current,
        _ => WorkspaceTarget::Name(workspace_name(words)?),
    })
}

fn workspace_name(words: &[&str]) -> Result<String> {
    ensure!(!words.is_empty(), "missing workspace name");
    let name = words
        .iter()
        .map(|word| weld_sway_config::input::literal(word))
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    ensure!(
        !name.is_empty() && !name.starts_with("--"),
        "unsupported or empty workspace name"
    );
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    fn parse(name: &str, source: &str) -> Result<Configuration> {
        parse_with_extensions(name, source, |_| bail!("unsupported i3 command"))
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
            Action::Workspace(I3WorkspaceRequest::Switch(WorkspaceTarget::Name(
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
            Action::Workspace(I3WorkspaceRequest::Switch(WorkspaceTarget::Number(
                "3: work".to_owned()
            )))
        );
        assert_eq!(
            config.bindings[1].1,
            Action::Workspace(I3WorkspaceRequest::MoveWindow(WorkspaceTarget::Number(
                "3".to_owned()
            )))
        );
        assert_eq!(
            config.bindings[2].1,
            Action::Workspace(I3WorkspaceRequest::Switch(WorkspaceTarget::Name(
                "next".to_owned()
            )))
        );
        assert_eq!(
            config.bindings[3].1,
            Action::Workspace(I3WorkspaceRequest::Switch(WorkspaceTarget::NextOnOutput))
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
        "},
        )
        .expect("supported subset");
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
            indoc! {"
                bindsym Mod4+Left focus left
                bindsym Mod4+Left focus right
            "},
            "bindsym Mod4+Left weld persistent toggle",
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
}
