//! Sway configuration interpretation for the i3 behavior assembly.

use crate::FocusWrapping;
use crate::workspace::{
    I3WorkspaceRequest, WorkspaceAssignment, WorkspaceSettings, WorkspaceTarget, number,
};
use anyhow::{Context, Result, bail, ensure};
use bevy::color::{Color, Srgba};
use weld_input::{GlobalShortcut, KeyboardKeymap};
use weld_ssd::{BorderStyle, FrameColors, SsdSettings};
use weld_sway_config::Statement;
pub use weld_sway_config::evaluation::{ConfigWarning, UnsupportedPolicy, unsupported};
use weld_sway_config::evaluation::{expand_variables, line_of};
use weld_tile::{Direction, SplitAxis, TileOperation, TileSettings};
use weld_window::fullscreen::{FullscreenAction, FullscreenMode};
use weld_window::pointer::WindowPointerSettings;

#[derive(Clone, Debug, PartialEq)]
pub enum Action<Extension = ()> {
    Focus(Direction),
    Move(Direction),
    Workspace(I3WorkspaceRequest),
    Tile(TileOperation),
    Layout(crate::I3LayoutRequest),
    Floating(Option<bool>),
    FocusModeToggle,
    FocusHierarchy(crate::I3FocusHierarchy),
    Sticky(Option<bool>),
    Border(Option<BorderStyle>),
    Fullscreen(FullscreenAction),
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
    pub pointer: WindowPointerSettings,
    pub decorations: SsdSettings,
    pub window_rules: crate::window_rules::WindowRules,
    pub warnings: Vec<ConfigWarning>,
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
            pointer: WindowPointerSettings {
                focus_follows_mouse: true,
                ..Default::default()
            },
            decorations: SsdSettings::default(),
            window_rules: Default::default(),
            warnings: Vec::new(),
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
    parse_with_policy(name, source, UnsupportedPolicy::Reject, extension)
}

/// Expand variables and compile supported behavior, retaining explicit warnings
/// when the caller allows unsupported features to be skipped.
pub fn parse_with_policy<Extension>(
    name: &str,
    source: &str,
    policy: UnsupportedPolicy,
    extension: impl Fn(&[&str]) -> Result<Extension>,
) -> Result<Configuration<Extension>> {
    let expanded = expand_variables(name, source)?;
    let source = expanded.as_str();
    let syntax = weld_sway_config::parse(name, source)?;
    let input = weld_sway_config::input::compile_with_policy(name, source, &syntax, policy)?;
    let mut config = Configuration {
        keymap: input.keymap,
        warnings: input.warnings,
        ..Default::default()
    };
    for binding in input.bindings {
        let words: Vec<_> = binding.command.iter().map(String::as_str).collect();
        let action = match action(&words, &extension) {
            Ok(action) => action,
            Err(error) => {
                policy.handle(
                    error.context(binding.command.join(" ")),
                    name,
                    binding.line,
                    &mut config.warnings,
                )?;
                continue;
            }
        };
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
        let line = line_of(source, statement)?;
        let result = if statement.name().text() == "bar" && statement.block().is_some() {
            apply_bar(&mut config, statement, name, source, policy)
        } else {
            apply(&mut config, statement)
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
    config.warnings.sort_by_key(|warning| warning.line);
    Ok(config)
}

fn apply_bar<Extension>(
    config: &mut Configuration<Extension>,
    statement: &Statement,
    name: &str,
    source: &str,
    policy: UnsupportedPolicy,
) -> Result<()> {
    if !statement.arguments().is_empty() {
        return Err(unsupported("named bars are not supported"));
    }
    let Some(block) = statement.block() else {
        return Ok(());
    };
    let mut command = None;
    for child in block.statements() {
        let line = line_of(source, child)?;
        if child.name().text() == "swaybar_command" && child.block().is_none() {
            let words = child
                .arguments()
                .iter()
                .map(|argument| argument.text())
                .collect::<Vec<_>>();
            command = Some(
                exec_command(&words).with_context(|| format!("{name}:{line}: swaybar_command"))?,
            );
        } else {
            policy.handle(
                unsupported(format!(
                    "unsupported bar setting: {}",
                    child.header().text()
                )),
                name,
                line,
                &mut config.warnings,
            )?;
        }
    }
    if let Some(command) = command {
        config.startup.push(StartupCommand {
            command,
            on_reload: false,
        });
    } else {
        policy.handle(
            unsupported("bar requires an explicit swaybar_command; no default bar was started"),
            name,
            line_of(source, statement)?,
            &mut config.warnings,
        )?;
    }
    Ok(())
}

fn apply<Extension>(config: &mut Configuration<Extension>, statement: &Statement) -> Result<()> {
    if statement.block().is_some() {
        return Err(unsupported(format!(
            "unsupported configuration block {}",
            statement.name().text()
        )));
    }
    let args: Vec<_> = statement
        .arguments()
        .iter()
        .map(|argument| argument.text())
        .collect();
    match (statement.name().text(), args.as_slice()) {
        ("for_window", [criteria, "border", style @ ..]) => {
            if style.iter().any(|word| word.contains([',', ';'])) {
                return Err(unsupported(
                    "window-rule command sequences are not supported",
                ));
            }
            config
                .window_rules
                .add_border(criteria, border_style(style)?)?;
        }
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
        ("default_orientation", ["auto"]) => {
            return Err(unsupported("automatic split orientation is not supported"));
        }
        ("default_orientation", [value]) => config.tiling.default_axis = axis(value)?,
        ("smart_gaps", [value @ ("on" | "off")]) => config.tiling.hide_solo_gaps = *value == "on",
        ("smart_borders", [value @ ("on" | "off")]) => {
            config.decorations.hide_solo_border = *value == "on"
        }
        ("default_border" | "new_window", style) => config.decorations.tiled = border_style(style)?,
        ("default_floating_border" | "new_float", style) => {
            config.decorations.floating = border_style(style)?
        }
        ("corner_radius", [radius]) => {
            config.decorations.corner_radius = radius
                .parse::<u16>()
                .context("corner radius must be an integer")?;
            ensure!(
                config.decorations.corner_radius <= 64,
                "corner radius must be between 0 and 64"
            );
        }
        (
            kind @ ("client.focused"
            | "client.unfocused"
            | "client.focused_inactive"
            | "client.placeholder"),
            values,
        ) => {
            let colors = frame_colors(values)?;
            match kind {
                "client.focused" => config.decorations.focused = colors,
                "client.focused_inactive" => config.decorations.focused_inactive = colors,
                "client.placeholder" => config.decorations.placeholder = colors,
                _ => config.decorations.unfocused = colors,
            }
        }
        ("floating_modifier", [value] | [value, "normal"]) => {
            config.pointer.modifier = Some(weld_sway_config::input::pointer_modifiers(value)?)
        }
        ("focus_follows_mouse", [value @ ("yes" | "no")]) => {
            config.pointer.focus_follows_mouse = *value == "yes";
        }
        ("focus_follows_mouse", ["always"])
        | ("smart_borders", ["no_gaps"])
        | ("smart_gaps", ["inverse_outer"]) => {
            return Err(unsupported(format!(
                "unsupported setting {}",
                statement.header().text()
            )));
        }
        ("focus_follows_mouse" | "smart_borders" | "smart_gaps", [_]) => {
            bail!("invalid value for {}", statement.name().text())
        }
        ("focus_wrapping", [value]) => {
            config.focus_wrapping = match *value {
                "no" => FocusWrapping::No,
                "yes" => FocusWrapping::Yes,
                "force" => FocusWrapping::Force,
                "workspace" => FocusWrapping::Workspace,
                _ => bail!("focus_wrapping must be no, yes, force or workspace"),
            };
        }
        ("include", _) => bail!("include is not supported yet; load a combined configuration file"),
        _ => {
            return Err(unsupported(format!(
                "unsupported directive {}",
                statement.name().text()
            )));
        }
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

fn border_style(words: &[&str]) -> Result<BorderStyle> {
    let (kind, width) = match words {
        ["none"] => return Ok(BorderStyle::None),
        [kind @ ("normal" | "pixel")] => (*kind, 3),
        [kind @ ("normal" | "pixel"), width] => (
            *kind,
            width
                .parse::<u16>()
                .context("border width must be an integer")?,
        ),
        _ => bail!("border style must be normal, pixel or none, with an optional pixel width"),
    };
    ensure!(width <= 64, "border width must be between 0 and 64");
    Ok(if kind == "normal" {
        BorderStyle::Normal(width)
    } else {
        BorderStyle::Pixel(width)
    })
}

fn frame_colors(words: &[&str]) -> Result<FrameColors> {
    let (border, background, foreground, indicator, child_border) = match words {
        [border, background, foreground] => {
            (*border, *background, *foreground, *border, *background)
        }
        [border, background, foreground, indicator] => {
            (*border, *background, *foreground, *indicator, *background)
        }
        [border, background, foreground, indicator, child_border] => {
            (*border, *background, *foreground, *indicator, *child_border)
        }
        _ => bail!(
            "client colors require border, background and text, with optional indicator and child-border colors"
        ),
    };
    let color = |value: &str| -> Result<Color> {
        ensure!(
            value.starts_with('#') && matches!(value.len(), 7 | 9),
            "colors must use #RRGGBB or #RRGGBBAA"
        );
        Ok(Srgba::hex(value)
            .context("invalid hexadecimal color")?
            .into())
    };
    Ok(FrameColors {
        border: color(border)?,
        background: color(background)?,
        foreground: color(foreground)?,
        indicator: color(indicator)?,
        child_border: color(child_border)?,
    })
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

fn layout_choice(word: &str) -> Result<crate::LayoutChoice> {
    use crate::LayoutChoice;
    use weld_tile::TileLayout;
    Ok(match word {
        "splith" => LayoutChoice::Layout(TileLayout::Split(SplitAxis::Horizontal)),
        "splitv" => LayoutChoice::Layout(TileLayout::Split(SplitAxis::Vertical)),
        "tabbed" => LayoutChoice::Layout(TileLayout::Tabbed),
        "stacking" | "stacked" => LayoutChoice::Layout(TileLayout::Stacked),
        "split" => LayoutChoice::Split,
        _ => bail!("unknown layout {word}"),
    })
}

fn layout_action(words: &[&str]) -> Result<crate::I3LayoutRequest> {
    use crate::{I3LayoutRequest, LayoutChoice};
    Ok(match words {
        ["toggle"] => I3LayoutRequest::Toggle,
        ["toggle", "all"] => I3LayoutRequest::ToggleAll,
        ["toggle", choices @ ..] if !choices.is_empty() => I3LayoutRequest::Cycle(
            choices
                .iter()
                .map(|word| layout_choice(word))
                .collect::<Result<_>>()?,
        ),
        ["default"] => I3LayoutRequest::Default,
        [word] => match layout_choice(word)? {
            LayoutChoice::Layout(layout) => I3LayoutRequest::Set(layout),
            LayoutChoice::Split => bail!("use layout toggle split or layout default"),
        },
        _ => bail!("layout requires a layout name or toggle sequence"),
    })
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use crate::{I3LayoutRequest, LayoutChoice};
    use weld_tile::TileLayout;

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

fn action<Extension>(
    words: &[&str],
    extension: &impl Fn(&[&str]) -> Result<Extension>,
) -> Result<Action<Extension>> {
    Ok(match words {
        ["focus", "tiling" | "floating"]
        | ["mode", ..]
        | ["move", "scratchpad"]
        | ["move", "workspace", "to", "output", ..] => {
            return Err(unsupported(format!(
                "unsupported command {}",
                words.join(" ")
            )));
        }
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
        ["layout", args @ ..] => Action::Layout(layout_action(args)?),
        ["splith"] | ["split", "h"] | ["split", "horizontal"] => {
            Action::Tile(TileOperation::Split(SplitAxis::Horizontal))
        }
        ["splitv"] | ["split", "v"] | ["split", "vertical"] => {
            Action::Tile(TileOperation::Split(SplitAxis::Vertical))
        }
        ["border", "toggle"] => Action::Border(None),
        ["fullscreen"] | ["fullscreen", "toggle"] => {
            Action::Fullscreen(FullscreenAction::Toggle(FullscreenMode::Normal))
        }
        ["fullscreen", "enable"] => {
            Action::Fullscreen(FullscreenAction::Enable(FullscreenMode::Normal))
        }
        ["fullscreen", "disable"] => Action::Fullscreen(FullscreenAction::Disable),
        ["border", style @ ..] => Action::Border(Some(border_style(style)?)),
        ["focus", "mode_toggle"] => Action::FocusModeToggle,
        ["sticky", "enable"] => Action::Sticky(Some(true)),
        ["sticky", "disable"] => Action::Sticky(Some(false)),
        ["sticky", "toggle"] => Action::Sticky(None),
        ["sticky", ..] => bail!("sticky mode must be enable, disable or toggle"),
        ["focus", "parent"] => Action::FocusHierarchy(crate::I3FocusHierarchy::Parent),
        ["focus", "child"] => Action::FocusHierarchy(crate::I3FocusHierarchy::Child),
        ["floating", "enable"] => Action::Floating(Some(true)),
        ["floating", "disable"] => Action::Floating(Some(false)),
        ["floating", "toggle"] => Action::Floating(None),
        ["floating", _] => bail!("floating mode must be enable, disable or toggle"),
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
        [
            "focus" | "move" | "floating" | "fullscreen" | "split" | "splith" | "splitv" | "resize"
            | "kill" | "reload" | "exit",
            ..,
        ] => {
            return Err(unsupported(format!(
                "unsupported command form {}",
                words.join(" ")
            )));
        }
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
            Color::srgb_u8(0x12, 0x34, 0x56)
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
            [2, 5, 8, 13]
        );
        assert!(config.pointer.focus_follows_mouse);
        assert!(config.keymap.is_none());
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
            "input type:keyboard xkb_layout nonexistent_weld_layout",
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
            Some(weld_input::PointerShortcutModifiers {
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
                Action::FocusHierarchy(crate::I3FocusHierarchy::Parent),
                Action::FocusHierarchy(crate::I3FocusHierarchy::Child)
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
            bindsym Mod1+b border toggle
            bindsym Mod1+n border none
        "},
        )
        .expect("frames");
        assert_eq!(config.decorations.tiled, BorderStyle::Pixel(4));
        assert_eq!(config.decorations.floating, BorderStyle::Normal(2));
        assert_eq!(
            config.decorations.focused.child_border,
            Color::srgb_u8(0x55, 0x66, 0x77)
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
