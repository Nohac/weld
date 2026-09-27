//! Native split-tree policy over durable managed windows.
//!
//! The tree is owned ECS state, independent of configuration syntax and UI.
//! [`TileCommands`] supplies typed edits; one exclusive management system applies
//! them and publishes geometry before presentation. Client occupancy never
//! determines tree membership: retained vacancies and hoisted windows keep slots.
//! This first slice has one workspace on the primary output. Other outputs,
//! floating overlays, tabbed/stacked layouts and fullscreen are separate policy.

mod layout;
mod operations;
mod plugin;

use std::collections::VecDeque;

use bevy::ecs::{component::Component, entity::Entity, resource::Resource, world::World};
use weld_app::output::OutputId;
use weld_window::WindowId;

const COMMAND_CAPACITY: usize = 256;

pub use plugin::TilePlugin;

/// Orientation of a split. Horizontal places children left to right.
// Improvement: expose Left/Right/Up/Down split requests in the native API, with
// direction selecting the insertion side. Translate Sway/i3 axis commands in
// the compatibility adapter. See docs/master-tiling.md, "Next improvements".
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SplitAxis {
    #[default]
    Horizontal,
    Vertical,
}

/// Direction in output-local logical coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Live tiling preferences. Replacing this resource preserves the current tree.
/// Gaps affect existing geometry; default axis affects only newly made splits.
#[derive(Resource, Clone, Copy, Debug, Eq, PartialEq)]
pub struct TileSettings {
    /// Logical pixels between siblings, clamped to the available space.
    pub inner_gap: u16,
    /// Logical pixels between the workspace and output edges.
    pub outer_gap: u16,
    /// Initial orientation for new workspace roots.
    pub default_axis: SplitAxis,
}

impl Default for TileSettings {
    fn default() -> Self {
        Self {
            inner_gap: 8,
            outer_gap: 8,
            default_axis: SplitAxis::Horizontal,
        }
    }
}

/// Session-stable container identity; never expose Bevy entities to persistence.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ContainerId(u64);

impl ContainerId {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Ordered tree edges and orientation, queryable but edited through commands.
#[derive(Component, Debug)]
pub struct TileContainer {
    id: ContainerId,
    axis: SplitAxis,
    children: Vec<TileChild>,
}

impl TileContainer {
    pub const fn id(&self) -> ContainerId {
        self.id
    }
    pub const fn axis(&self) -> SplitAxis {
        self.axis
    }
    pub fn children(&self) -> impl Iterator<Item = (Entity, f32)> + '_ {
        self.children
            .iter()
            .map(|child| (child.entity, child.weight))
    }
}

#[derive(Clone, Copy, Debug)]
struct TileChild {
    entity: Entity,
    weight: f32,
}

/// Parent edge of a window or container. Structural edits preserve both sides.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct TileParent(Entity);

impl TileParent {
    pub const fn entity(self) -> Entity {
        self.0
    }
}

/// The root workspace and its current output association.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct TileWorkspace {
    pub output: OutputId,
}

/// Commands target stable window identities, not UI nodes or client surfaces.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TileCommand {
    pub window: WindowId,
    pub operation: TileOperation,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TileOperation {
    /// Requests client close without removing a retained window slot.
    Close,
    /// Wrap this window in a split for the next admission, or orient its sole-child parent.
    Split(SplitAxis),
    Focus(Direction),
    /// Swap leaf positions with the directional neighbor, including across splits.
    Move(Direction),
    /// Adjust this branch against a sibling by a fraction of their combined share.
    Resize {
        axis: SplitAxis,
        fraction: f32,
    },
}

/// Bounded, ordered native operations. Producers can retry on a full queue.
#[derive(Resource, Default)]
pub struct TileCommands(VecDeque<QueuedCommand>);

enum QueuedCommand {
    Window(TileCommand),
    Focused(TileOperation),
}

impl TileCommands {
    /// Applies pending operations before another subsystem action in an ordered
    /// shell batch (for example focus-left followed by hoist). Uses the current
    /// workspace bounds; ordinary management still owns admission/output updates.
    /// Before the first layout is available, commands remain queued.
    pub fn flush(world: &mut World) {
        plugin::flush_commands(world);
    }

    pub fn push(&mut self, command: TileCommand) -> Result<(), TileCommand> {
        if self.0.len() == COMMAND_CAPACITY {
            return Err(command);
        }
        self.0.push_back(QueuedCommand::Window(command));
        Ok(())
    }

    /// Resolves focus when this operation executes, preserving sequential
    /// keyboard navigation even when several inputs arrive in one frame.
    pub fn push_focused(&mut self, operation: TileOperation) -> Result<(), TileOperation> {
        if self.0.len() == COMMAND_CAPACITY {
            return Err(operation);
        }
        self.0.push_back(QueuedCommand::Focused(operation));
        Ok(())
    }
}

#[derive(Resource, Default)]
struct TileState {
    root: Option<Entity>,
    next_id: u64,
}

#[cfg(test)]
mod tests;
