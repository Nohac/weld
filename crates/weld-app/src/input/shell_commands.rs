//! Typed shell effects, independent of which configuration or UI requests them.

use std::collections::VecDeque;

use bevy::ecs::{resource::Resource, world::World};
use weld_core::runtime::{HostCommand, OutputScaleAdjustment};

/// Distribution-requested host work. Commands run through the normal host
/// boundary; configuration parsing never executes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellCommand {
    Launch {
        program: String,
        arguments: Vec<String>,
    },
    Exit,
    IncreaseOutputScale,
    DecreaseOutputScale,
    MatchOutputPhysicalScale,
}

impl ShellCommand {
    pub const fn requires_drm(&self) -> bool {
        matches!(
            self,
            Self::IncreaseOutputScale | Self::DecreaseOutputScale | Self::MatchOutputPhysicalScale
        )
    }

    fn into_host(self) -> HostCommand {
        match self {
            Self::Launch { program, arguments } => HostCommand::Launch {
                program: program.into(),
                arguments: arguments.into_iter().map(Into::into).collect(),
            },
            Self::Exit => HostCommand::Exit,
            Self::IncreaseOutputScale => {
                HostCommand::AdjustOutputScale(OutputScaleAdjustment::Increase)
            }
            Self::DecreaseOutputScale => {
                HostCommand::AdjustOutputScale(OutputScaleAdjustment::Decrease)
            }
            Self::MatchOutputPhysicalScale => HostCommand::MatchOutputPhysicalScale,
        }
    }
}

/// Bounded, ordered shell effects. Adding this resource installs no key bindings.
#[derive(Resource, Default)]
pub struct ShellCommands(VecDeque<ShellCommand>);

impl ShellCommands {
    pub fn push(&mut self, command: ShellCommand) -> Result<(), ShellCommand> {
        if self.0.len() == 256 {
            return Err(command);
        }
        self.0.push_back(command);
        Ok(())
    }
}

pub(super) fn take_commands(world: &mut World) -> Vec<HostCommand> {
    world
        .get_resource_mut::<ShellCommands>()
        .map(|mut commands| commands.0.drain(..).map(ShellCommand::into_host).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_effects_drain_once_in_order_without_installing_bindings() {
        let mut world = World::new();
        world.init_resource::<ShellCommands>();
        for command in [
            ShellCommand::Launch {
                program: "foot".into(),
                arguments: vec!["--title".into(), "sample".into()],
            },
            ShellCommand::IncreaseOutputScale,
            ShellCommand::Exit,
        ] {
            world
                .resource_mut::<ShellCommands>()
                .push(command)
                .expect("queue capacity");
        }
        assert_eq!(
            take_commands(&mut world),
            vec![
                HostCommand::Launch {
                    program: "foot".into(),
                    arguments: vec!["--title".into(), "sample".into()],
                },
                HostCommand::AdjustOutputScale(OutputScaleAdjustment::Increase),
                HostCommand::Exit
            ]
        );
        assert!(take_commands(&mut world).is_empty());
    }
}
