//! Distribution action lowering and fallible runtime resource preparation.

use super::rules::ConfiguredRules;
use anyhow::{Context, Result};
use weld_app::input::ShellCommand;
use weld_config::UnsupportedPolicy;
use weld_input::KeyboardKeymap;
use weld_ssd::SsdSettings;
use weld_sway_config::lowering as sway;
use weld_sway_config::typed::WeldCommand;
#[cfg(test)]
use weld_tile_config::{Direction, SplitAxis, TileOperation};
use weldwm_config as model;

pub(super) type Action = model::Action<DistributionAction>;
/// Candidate with fallible native resources prepared before publication.
#[derive(Clone, Debug)]
pub(super) struct PreparedConfiguration {
    pub settings: model::Configuration<DistributionAction>,
    pub keymap: Option<KeyboardKeymap>,
    pub window_rules: ConfiguredRules,
    pub decorations: SsdSettings,
}
#[derive(Clone, Debug, PartialEq)]
pub(super) enum DistributionAction {
    RevokeAccess,
    ToggleRemoteControl,
    HoldLocalInteraction,
    ToggleLocalInteraction,
    Hoist,
    OutputTopology,
    ExclusiveFullscreen,
    Shell(ShellCommand),
}

pub(super) fn parse(name: &str, source: &str) -> Result<PreparedConfiguration> {
    let config = sway::parse_with_policy(name, source, UnsupportedPolicy::Warn, extension)?;
    for (chord, action) in config
        .bindings
        .iter()
        .chain(config.modes.iter().flat_map(|mode| &mode.bindings))
    {
        anyhow::ensure!(
            chord.trigger.is_none()
                == matches!(
                    action,
                    Action::Extension(DistributionAction::HoldLocalInteraction)
                ),
            "modifier-only chords require weld control interact hold"
        );
    }
    let keymap = config
        .keymap
        .as_ref()
        .map(KeyboardKeymap::compile)
        .transpose()
        .with_context(|| format!("{name}: keyboard configuration"))?;
    let mut window_rules = ConfiguredRules::default();
    for rule in &config.window_rules {
        window_rules.add(rule.value.clone()).with_context(|| {
            format!(
                "{}:{}: window rule",
                rule.location.source, rule.location.line
            )
        })?;
    }
    let decorations = config.decorations.clone().into();
    Ok(PreparedConfiguration {
        settings: config,
        keymap,
        window_rules,
        decorations,
    })
}

fn extension(command: &WeldCommand) -> Result<DistributionAction> {
    Ok(match command {
        WeldCommand::RevokeAccess => DistributionAction::RevokeAccess,
        WeldCommand::ToggleRemoteControl => DistributionAction::ToggleRemoteControl,
        WeldCommand::HoldLocalInteraction => DistributionAction::HoldLocalInteraction,
        WeldCommand::ToggleLocalInteraction => DistributionAction::ToggleLocalInteraction,
        WeldCommand::Hoist => DistributionAction::Hoist,
        WeldCommand::OutputTopology => DistributionAction::OutputTopology,
        WeldCommand::ExclusiveFullscreen => DistributionAction::ExclusiveFullscreen,
        WeldCommand::IncreaseScale => DistributionAction::Shell(ShellCommand::IncreaseOutputScale),
        WeldCommand::DecreaseScale => DistributionAction::Shell(ShellCommand::DecreaseOutputScale),
        WeldCommand::PhysicalScale => {
            DistributionAction::Shell(ShellCommand::MatchOutputPhysicalScale)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_input::{GlobalShortcut, GlobalShortcutModifiers, KeyCode};

    #[test]
    fn native_preparation_rejects_invalid_keymaps_and_rule_patterns() {
        for source in [
            "input type:keyboard xkb_layout nonexistent_weld_layout",
            "for_window [title=\"(?=unsupported)\"] border pixel 2",
        ] {
            assert!(parse("candidate.conf", source).is_err(), "{source}");
        }
    }

    #[test]
    fn default_navigation_uses_letters_without_a_launch_conflict() {
        let config = parse("default", include_str!("../../examples/master.sway.config"))
            .expect("config")
            .settings;
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
                    Action::Move(direction),
                ),
            ] {
                let binding = config
                    .bindings
                    .iter()
                    .find(|(chord, _)| {
                        GlobalShortcut::from(*chord) == GlobalShortcut::new(key, modifiers)
                    })
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
            assert!(
                config
                    .bindings
                    .iter()
                    .map(|(chord, action)| (GlobalShortcut::from(*chord), action.clone()))
                    .collect::<Vec<_>>()
                    .contains(&(
                        GlobalShortcut::new(key, modifier),
                        Action::Tile(TileOperation::Split(axis))
                    ))
            );
        }
        assert!(
            config
                .bindings
                .iter()
                .map(|(chord, action)| (GlobalShortcut::from(*chord), action.clone()))
                .collect::<Vec<_>>()
                .contains(&(
                    GlobalShortcut::new(
                        KeyCode::KeyQ,
                        GlobalShortcutModifiers {
                            alt: true,
                            shift: true,
                            ..Default::default()
                        }
                    ),
                    Action::Tile(TileOperation::Close)
                ))
        );
        assert!(!config.bindings.iter().any(|(chord, _)| matches!(
            GlobalShortcut::from(*chord).trigger,
            Some(
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
            )
        )));
        assert_eq!(
            extension(&WeldCommand::Hoist).expect("extension"),
            DistributionAction::Hoist
        );
        assert!(parse("invalid", "bindsym h weld unknown").is_err());
    }
}
