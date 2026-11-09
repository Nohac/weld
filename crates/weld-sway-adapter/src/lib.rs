//! Sway compatibility lowering and preparation for Weld's desktop assembly.
//!
//! Parsing owns spelling and local argument validation. This adapter consumes
//! typed syntax, applies compatibility policy, and prepares a complete candidate
//! before Master publishes any settings or queues startup effects.

use weld_i3_config::{
    Field, FocusWrapping, I3FocusHierarchy, I3LayoutRequest, I3ResizeRequest, I3WorkspaceRequest,
    LayoutChoice, RuleAction, RuleDefinition, RuleEffect, WorkspaceAssignment, WorkspaceNavigation,
    WorkspaceTarget,
};
use weld_ssd_config::BorderStyle;
mod font;
use weld_config::{Located, UnsupportedPolicy, unsupported};
use weld_master_config::{Action, BindingMode, Configuration, StartupCommand};
use weld_sway_config::typed::WeldCommand;

use anyhow::{Result, bail, ensure};
use std::collections::HashSet;

use weld_input_config::{
    CursorThemeSettings, FingerButtonMap, InputDeviceSelector, KeymapConfig, PointerDeviceRule,
    PointerDeviceSettings, PointerShortcutModifiers, Shortcut, TouchpadClickMethod,
    TouchpadPreferences,
};
use weld_output::{OutputScaleRule, OutputSelector};
use weld_ssd_config::{FrameColors, Rgba};
use weld_sway_config::typed as syntax;
use weld_tile_config::{Direction, SplitAxis, TileLayout, TileOperation};
use weld_window_config::{FullscreenAction, FullscreenMode, PersistenceAction, PersistenceMatch};

/// Parse and lower a native candidate, rejecting unsupported features.
pub fn parse_with_extensions<E>(
    name: &str,
    source: &str,
    extension: impl Fn(&WeldCommand) -> Result<E>,
) -> Result<Configuration<E>> {
    parse_with_policy(name, source, UnsupportedPolicy::Reject, extension)
}

/// Parse and lower without preparing runtime resources or executing effects.
pub fn parse_with_policy<E>(
    name: &str,
    source: &str,
    policy: UnsupportedPolicy,
    extension: impl Fn(&WeldCommand) -> Result<E>,
) -> Result<Configuration<E>> {
    lower(syntax::parse(name, source, policy)?, policy, extension)
}

/// Lower a typed document into feature-owned native configuration contracts.
pub fn lower<E>(
    document: syntax::Document,
    policy: UnsupportedPolicy,
    extension: impl Fn(&WeldCommand) -> Result<E>,
) -> Result<Configuration<E>> {
    let mut config = Configuration {
        warnings: document.warnings,
        ..Default::default()
    };
    let mut keymap = KeymapConfig::default();
    let mut keymap_configured = false;
    let mut bindings = HashSet::new();
    let mut mode_names = HashSet::new();
    let mut mode_references = Vec::new();
    for node in document.directives {
        let result = (|| -> Result<()> {
            match node.value {
                syntax::Directive::Setting(setting) => apply(&mut config, setting)?,
                syntax::Directive::Binding(binding) => {
                    let (chord, action) = lower_binding(binding, &extension)?;
                    ensure!(bindings.insert(chord), "duplicate key binding");
                    if let Action::Mode(name) = &action {
                        mode_references.push((name.clone(), node.location.clone()));
                    }
                    config.bindings.push((chord, action));
                }
                syntax::Directive::Mode(mode) => {
                    let (name, pango_markup) = match mode.header {
                        syntax::ModeHeader::Plain(name) => (name.into_string(), false),
                        syntax::ModeHeader::Markup(name) => (name.into_string(), true),
                    };
                    if name != "default" {
                        ensure!(mode_names.insert(name.clone()), "duplicate binding mode");
                    }
                    let mut lowered = BindingMode {
                        name: name.clone(),
                        pango_markup,
                        bindings: Vec::new(),
                    };
                    let mut mode_chords = HashSet::new();
                    for binding in mode.bindings {
                        let result =
                            lower_binding(binding.value, &extension).and_then(|(chord, action)| {
                                let chords = if name == "default" {
                                    &mut bindings
                                } else {
                                    &mut mode_chords
                                };
                                ensure!(chords.insert(chord), "duplicate key binding");
                                Ok((chord, action))
                            });
                        match result {
                            Ok(lowered_binding) => {
                                if let Action::Mode(name) = &lowered_binding.1 {
                                    mode_references.push((name.clone(), binding.location.clone()));
                                }
                                lowered.bindings.push(lowered_binding);
                            }
                            Err(error) => policy.handle(
                                error,
                                &binding.location.source,
                                binding.location.line,
                                &mut config.warnings,
                            )?,
                        }
                    }
                    if name == "default" {
                        config.bindings.extend(lowered.bindings);
                    } else {
                        config.modes.push(lowered);
                    }
                }
                syntax::Directive::Input(selector, property) => {
                    keymap_configured |=
                        input(&mut config.pointer_devices, &mut keymap, selector, property)?
                }
                syntax::Directive::Cursor(theme, size) => {
                    config.cursor = Some(CursorThemeSettings {
                        theme: theme.0,
                        size: size.map(|value| value.0),
                    })
                }
                syntax::Directive::OutputScale(selector, scale) => {
                    config.outputs.scales.push(OutputScaleRule {
                        selector: if selector.0 == "*" {
                            OutputSelector::All
                        } else {
                            OutputSelector::Named(selector.0)
                        },
                        scale: weld_output::OutputScale::new(scale.0)?,
                    })
                }
                syntax::Directive::Startup { command, on_reload } => {
                    config.startup.push(StartupCommand {
                        command: command.0,
                        on_reload,
                    })
                }
                syntax::Directive::Rule(rule) => config.window_rules.push(Located {
                    value: lower_rule(rule)?,
                    location: node.location.clone(),
                }),
            }
            Ok(())
        })();
        if let Err(error) = result {
            policy.handle(
                error,
                &node.location.source,
                node.location.line,
                &mut config.warnings,
            )?;
        }
    }
    for (_, action) in &config.bindings {
        if let Action::Workspace(I3WorkspaceRequest::Navigate {
            target: WorkspaceTarget::Name(name) | WorkspaceTarget::Number(name),
            operation: WorkspaceNavigation::Switch,
            ..
        }) = action
            && !config.workspaces.initial_names.contains(name)
        {
            config.workspaces.initial_names.push(name.clone());
        }
    }
    for (name, location) in mode_references {
        ensure!(
            name == "default" || config.modes.iter().any(|mode| mode.name == name),
            "{}:{}: binding targets undefined mode {name:?}",
            location.source,
            location.line
        );
    }
    if keymap_configured {
        config.keymap = Some(keymap);
    }
    config.warnings.sort_by_key(|warning| warning.line);
    Ok(config)
}

fn lower_binding<E>(
    binding: syntax::Binding,
    extension: &impl Fn(&WeldCommand) -> Result<E>,
) -> Result<(Shortcut, Action<E>)> {
    let modifiers = modifiers(binding.chord.modifiers);
    let shortcut = Shortcut {
        trigger: binding.chord.key.map(key),
        modifiers,
    };
    Ok((shortcut, action(binding.command, extension)?))
}

fn modifiers(value: syntax::Modifiers) -> PointerShortcutModifiers {
    let mut result = PointerShortcutModifiers::default();
    for modifier in value.0 {
        match modifier {
            syntax::Modifier::Control => result.control = true,
            syntax::Modifier::Alt => result.alt = true,
            syntax::Modifier::Shift => result.shift = true,
            syntax::Modifier::Super => result.super_key = true,
        }
    }
    result
}

fn direction(value: syntax::Direction) -> Direction {
    match value {
        syntax::Direction::Left => Direction::Left,
        syntax::Direction::Right => Direction::Right,
        syntax::Direction::Up => Direction::Up,
        syntax::Direction::Down => Direction::Down,
    }
}
fn axis(value: syntax::Axis) -> SplitAxis {
    match value {
        syntax::Axis::Horizontal => SplitAxis::Horizontal,
        syntax::Axis::Vertical => SplitAxis::Vertical,
    }
}
fn toggle(value: syntax::Toggle) -> Option<bool> {
    match value {
        syntax::Toggle::Enable => Some(true),
        syntax::Toggle::Disable => Some(false),
        syntax::Toggle::Toggle => None,
    }
}
fn border(value: syntax::Border) -> BorderStyle {
    match value {
        syntax::Border::Normal(width) => BorderStyle::Normal(width.value()),
        syntax::Border::Pixel(width) => BorderStyle::Pixel(width.value()),
        syntax::Border::None => BorderStyle::None,
    }
}
fn workspace(value: syntax::WorkspaceTarget) -> WorkspaceTarget {
    match value {
        syntax::WorkspaceTarget::Name(name) => WorkspaceTarget::Name(name.into_string()),
        syntax::WorkspaceTarget::Number(name) => WorkspaceTarget::Number(name.into_string()),
        syntax::WorkspaceTarget::Next => WorkspaceTarget::Next,
        syntax::WorkspaceTarget::Previous => WorkspaceTarget::Previous,
        syntax::WorkspaceTarget::NextOnOutput => WorkspaceTarget::NextOnOutput,
        syntax::WorkspaceTarget::PreviousOnOutput => WorkspaceTarget::PreviousOnOutput,
        syntax::WorkspaceTarget::BackAndForth => WorkspaceTarget::BackAndForth,
        syntax::WorkspaceTarget::Current => WorkspaceTarget::Current,
    }
}
fn named_workspace(value: syntax::WorkspaceTarget) -> Result<WorkspaceTarget> {
    let target = workspace(value);
    ensure!(
        matches!(
            target,
            WorkspaceTarget::Name(_) | WorkspaceTarget::Number(_)
        ),
        "window rules require a workspace name or number"
    );
    Ok(target)
}
fn persistence(value: syntax::Persistence) -> PersistenceAction {
    let field = match value.field {
        syntax::PersistenceField::Auto => PersistenceMatch::Auto,
        syntax::PersistenceField::AppId => PersistenceMatch::AppId,
        syntax::PersistenceField::Class => PersistenceMatch::Class,
        syntax::PersistenceField::Instance => PersistenceMatch::Instance,
        syntax::PersistenceField::Title => PersistenceMatch::Title,
    };
    match value.operation {
        syntax::Toggle::Enable => PersistenceAction::Enable(field),
        syntax::Toggle::Disable => PersistenceAction::Disable,
        syntax::Toggle::Toggle => PersistenceAction::Toggle(field),
    }
}
fn layout_choice(value: syntax::LayoutChoice) -> LayoutChoice {
    match value {
        syntax::LayoutChoice::Horizontal => {
            LayoutChoice::Layout(TileLayout::Split(SplitAxis::Horizontal))
        }
        syntax::LayoutChoice::Vertical => {
            LayoutChoice::Layout(TileLayout::Split(SplitAxis::Vertical))
        }
        syntax::LayoutChoice::Tabbed => LayoutChoice::Layout(TileLayout::Tabbed),
        syntax::LayoutChoice::Stacked => LayoutChoice::Layout(TileLayout::Stacked),
        syntax::LayoutChoice::Split => LayoutChoice::Split,
    }
}

fn action<E>(
    value: syntax::Command,
    extension: &impl Fn(&WeldCommand) -> Result<E>,
) -> Result<Action<E>> {
    Ok(match value {
        syntax::Command::MoveWorkspaceToOutput(_) => {
            return Err(unsupported("moving workspaces to outputs is not supported"));
        }
        syntax::Command::Workspace(target) => {
            Action::Workspace(I3WorkspaceRequest::switch(workspace(target)))
        }
        syntax::Command::WorkspaceNoBack(target) => Action::Workspace(
            I3WorkspaceRequest::switch(workspace(target)).without_auto_back_and_forth(),
        ),
        syntax::Command::MoveWorkspace(target) => {
            Action::Workspace(I3WorkspaceRequest::move_window(workspace(target)))
        }
        syntax::Command::MoveWorkspaceNoBack(target) => Action::Workspace(
            I3WorkspaceRequest::move_window(workspace(target)).without_auto_back_and_forth(),
        ),
        syntax::Command::Rename(rename) => Action::Workspace(I3WorkspaceRequest::Rename {
            source: rename.source.map(|name| name.into_string()),
            name: rename.name.into_string(),
        }),
        syntax::Command::Exec(command) => Action::Exec(command.0),
        syntax::Command::Exit => Action::Exit,
        syntax::Command::Mode(name) => Action::Mode(name.into_string()),
        syntax::Command::Layout(layout) => Action::Layout(match layout {
            syntax::Layout::Default => I3LayoutRequest::Default,
            syntax::Layout::Toggle => I3LayoutRequest::Toggle,
            syntax::Layout::ToggleAll => I3LayoutRequest::ToggleAll,
            syntax::Layout::Cycle(choices) => {
                I3LayoutRequest::Cycle(choices.into_iter().map(layout_choice).collect())
            }
            syntax::Layout::Set(choice) => {
                let LayoutChoice::Layout(layout) = layout_choice(choice) else {
                    bail!("split restoration requires a layout cycle")
                };
                I3LayoutRequest::Set(layout)
            }
        }),
        syntax::Command::Split(value) => Action::Tile(TileOperation::Split(axis(value))),
        syntax::Command::BorderToggle => Action::Border(None),
        syntax::Command::Border(value) => Action::Border(Some(border(value))),
        syntax::Command::Persistent(value) => Action::Persistent(persistence(value)),
        syntax::Command::Fullscreen(value) => Action::Fullscreen(match value {
            syntax::Toggle::Enable => FullscreenAction::Enable(FullscreenMode::Normal),
            syntax::Toggle::Disable => FullscreenAction::Disable,
            syntax::Toggle::Toggle => FullscreenAction::Toggle(FullscreenMode::Normal),
        }),
        syntax::Command::FocusModeToggle => Action::FocusModeToggle,
        syntax::Command::FocusParent => Action::FocusHierarchy(I3FocusHierarchy::Parent),
        syntax::Command::FocusChild => Action::FocusHierarchy(I3FocusHierarchy::Child),
        syntax::Command::Focus(value) => Action::Focus(direction(value)),
        syntax::Command::Move(value) => Action::Move(direction(value)),
        syntax::Command::Sticky(value) => Action::Sticky(toggle(value)),
        syntax::Command::Floating(value) => Action::Floating(toggle(value)),
        syntax::Command::Resize(value) => {
            let mut resize = I3ResizeRequest {
                axis: match value.dimension {
                    syntax::Dimension::Width => SplitAxis::Horizontal,
                    syntax::Dimension::Height => SplitAxis::Vertical,
                },
                pixels: None,
                fraction: None,
            };
            let sign = if value.kind == syntax::ResizeKind::Grow {
                1.0
            } else {
                -1.0
            };
            for amount in value.amounts {
                match amount.unit {
                    syntax::Unit::Pixels => resize.pixels = Some(f32::from(amount.value) * sign),
                    syntax::Unit::Percentage => {
                        resize.fraction = Some(f32::from(amount.value) * sign / 100.0)
                    }
                }
            }
            Action::Resize(resize)
        }
        syntax::Command::Kill => Action::Tile(TileOperation::Close),
        syntax::Command::Reload => Action::Reload,
        syntax::Command::Position(_) => {
            return Err(unsupported(
                "absolute positions currently require a window rule",
            ));
        }
        syntax::Command::Weld(command) => Action::Extension(extension(&command)?),
    })
}

fn apply<E>(config: &mut Configuration<E>, setting: syntax::Setting) -> Result<()> {
    match setting {
        syntax::Setting::GapInner(value) => config.tiling.inner_gap = value.value(),
        syntax::Setting::GapOuter(value) => config.tiling.outer_gap = value.value(),
        syntax::Setting::DefaultAxis(value) => {
            config.tiling.default_axis = match value {
                syntax::Orientation::Horizontal => SplitAxis::Horizontal,
                syntax::Orientation::Vertical => SplitAxis::Vertical,
            }
        }
        syntax::Setting::SmartGaps(value) => {
            config.tiling.hide_solo_gaps = value == syntax::OnOff::On
        }
        syntax::Setting::SmartBorders(value) => {
            config.decorations.hide_solo_border = value == syntax::OnOff::On
        }
        syntax::Setting::TiledBorder(value) => config.decorations.tiled = border(value),
        syntax::Setting::FloatingBorder(value) => config.decorations.floating = border(value),
        syntax::Setting::Font(value) => config.decorations.title_font = font::parse(&value.0)?,
        syntax::Setting::Radius(value) => config.decorations.corner_radius = value.value(),
        syntax::Setting::Colors(palette, values) => {
            let target = match palette {
                syntax::Palette::Focused => &mut config.decorations.focused,
                syntax::Palette::Unfocused => &mut config.decorations.unfocused,
                syntax::Palette::FocusedInactive => &mut config.decorations.focused_inactive,
                syntax::Palette::Urgent => &mut config.decorations.urgent,
                syntax::Palette::Placeholder => &mut config.decorations.placeholder,
                syntax::Palette::PersistentFocused => &mut config.decorations.persistent_focused,
                syntax::Palette::PersistentUnfocused => {
                    &mut config.decorations.persistent_unfocused
                }
            };
            let color = |value: syntax::Color| {
                let [r, g, b, a] = value.0;
                Rgba::srgba_u8(r, g, b, a)
            };
            *target = FrameColors {
                border: color(values.border),
                background: color(values.background),
                foreground: color(values.text),
                indicator: color(values.indicator),
                child_border: color(values.child_border),
            };
        }
        syntax::Setting::FloatingModifier(value) => {
            config.pointer.modifier = Some(modifiers(value))
        }
        syntax::Setting::FocusFollowsMouse(value) => {
            config.pointer.focus_follows_mouse = value == syntax::YesNo::Yes
        }
        syntax::Setting::FocusWrapping(value) => {
            config.focus_wrapping = match value {
                syntax::FocusWrapping::No => FocusWrapping::No,
                syntax::FocusWrapping::Yes => FocusWrapping::Yes,
                syntax::FocusWrapping::Force => FocusWrapping::Force,
                syntax::FocusWrapping::Workspace => FocusWrapping::Workspace,
            }
        }
        syntax::Setting::AutoBack(value) => {
            config.workspaces.auto_back_and_forth = value == syntax::YesNo::Yes
        }
        syntax::Setting::WorkspaceOutput(value) => {
            let name = value.name.into_string();
            let outputs = value
                .outputs
                .into_iter()
                .map(|value| value.0)
                .collect::<Vec<_>>();
            ensure!(
                outputs.iter().all(|value| !value.is_empty()),
                "output name must not be empty"
            );
            if !config
                .workspaces
                .assignments
                .iter()
                .any(|entry| entry.workspace == name)
            {
                config.workspaces.assignments.push(WorkspaceAssignment {
                    workspace: name,
                    outputs,
                });
            }
        }
    }
    Ok(())
}

fn input(
    devices: &mut PointerDeviceSettings,
    keymap: &mut KeymapConfig,
    selector: syntax::InputSelector,
    property: syntax::InputProperty,
) -> Result<bool> {
    if let syntax::InputProperty::Xkb(field, value) = property {
        if !matches!(
            selector,
            syntax::InputSelector::All | syntax::InputSelector::Keyboard
        ) {
            return Err(unsupported(
                "keyboard settings support * and type:keyboard selectors",
            ));
        }
        let target = match field {
            syntax::XkbField::Rules => &mut keymap.rules,
            syntax::XkbField::Model => &mut keymap.model,
            syntax::XkbField::Layout => &mut keymap.layout,
            syntax::XkbField::Variant => &mut keymap.variant,
            syntax::XkbField::Options => &mut keymap.options,
        };
        *target = value.0;
        return Ok(true);
    }
    let selector = match selector {
        syntax::InputSelector::All => InputDeviceSelector::All,
        syntax::InputSelector::Touchpad => InputDeviceSelector::Touchpad,
        syntax::InputSelector::Identifier(value) => InputDeviceSelector::Identifier(value),
        _ => return Err(unsupported("unsupported input selector")),
    };
    let mut preferences = TouchpadPreferences::default();
    match property {
        syntax::InputProperty::Tap(value) => {
            preferences.tap = Some(value == syntax::Enabled::Enabled)
        }
        syntax::InputProperty::TapMap(value) => {
            preferences.tap_button_map = Some(match value {
                syntax::TapMap::Lrm => FingerButtonMap::LeftRightMiddle,
                syntax::TapMap::Lmr => FingerButtonMap::LeftMiddleRight,
            })
        }
        syntax::InputProperty::ClickMethod(value) => {
            preferences.click_method = Some(match value {
                syntax::ClickMethod::None => TouchpadClickMethod::None,
                syntax::ClickMethod::ButtonAreas => TouchpadClickMethod::ButtonAreas,
                syntax::ClickMethod::Clickfinger => TouchpadClickMethod::Clickfinger,
            })
        }
        syntax::InputProperty::Xkb(..) => bail!("XKB settings require keyboard preparation"),
    }
    devices.0.push(PointerDeviceRule {
        selector,
        preferences,
    });
    Ok(false)
}

fn lower_rule(rule: syntax::WindowRule) -> Result<RuleDefinition> {
    let criteria = rule
        .criteria
        .into_iter()
        .map(|criterion| {
            let field = match criterion.field {
                syntax::CriterionField::AppId => Field::AppId,
                syntax::CriterionField::Class => Field::Class,
                syntax::CriterionField::Instance => Field::Instance,
                syntax::CriterionField::Title => Field::Title,
                syntax::CriterionField::All => bail!("all does not accept a pattern"),
            };
            Ok((field, criterion.pattern))
        })
        .collect::<Result<_>>()?;
    let effect = match rule.effect {
        syntax::RuleEffect::Assign(target) => RuleEffect::Assign(named_workspace(target)?),
        syntax::RuleEffect::NoFocus => RuleEffect::NoFocus,
        syntax::RuleEffect::Commands(commands) => RuleEffect::Commands(
            commands
                .into_iter()
                .map(|command| {
                    Ok(match command {
                        syntax::Command::Persistent(value) => {
                            let action = persistence(value);
                            ensure!(
                                !matches!(action, PersistenceAction::Toggle(_)),
                                "window rules require persistence enable or disable"
                            );
                            RuleAction::Persistent(action)
                        }
                        syntax::Command::Border(value) => RuleAction::Border(border(value)),
                        syntax::Command::Floating(value) if value != syntax::Toggle::Toggle => {
                            RuleAction::Floating(value == syntax::Toggle::Enable)
                        }
                        syntax::Command::MoveWorkspace(value) => {
                            RuleAction::Workspace(named_workspace(value)?)
                        }
                        syntax::Command::Position(value) => {
                            let position = [value.x as f32, value.y as f32];
                            ensure!(
                                position.iter().all(|value| value.is_finite()),
                                "window position must be finite"
                            );
                            RuleAction::Position(position)
                        }
                        _ => return Err(unsupported("unsupported window-rule action")),
                    })
                })
                .collect::<Result<_>>()?,
        ),
    };
    Ok(RuleDefinition { criteria, effect })
}

mod keys;
use keys::key;
#[cfg(test)]
mod tests;
