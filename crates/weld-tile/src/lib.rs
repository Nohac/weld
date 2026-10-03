//! Native split-tree policy over durable managed windows.
//!
//! Typed systems admit windows, edit the tree and publish geometry before
//! presentation. [`TileRequest`] observers preserve ordered shell actions;
//! [`TileCommands`] buffers operations until a workspace is available.
//! Retained vacancies and hoisted windows keep their layout slots.
//! Each managed workspace owns a tree in its assigned output's coordinates.

mod floating;
mod history;
mod layout;
mod operations;
mod plugin;
mod structural;
mod workspace;

use bevy::ecs::{
    component::Component,
    entity::Entity,
    event::Event,
    resource::Resource,
    schedule::SystemSet,
    system::{Command, command},
    world::CommandQueue,
};
use weld_window::WindowId;

const COMMAND_CAPACITY: usize = 256;
const MAX_DEPTH: usize = 64;

pub use history::TileFocusHistory;
pub use plugin::TilePlugin;

/// Management publication points. Deferred edits finish between each set.
#[derive(SystemSet, Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TileSystems {
    /// Focus policies recover against the previous tree before removal compacts it.
    RecoverFocus,
    Prepare,
    Commands,
    /// Distribution actions run after admission and before final layout.
    Actions,
    Layout,
}

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

/// Insertion position relative to another child in the same container.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TileSide {
    Before,
    After,
}

/// Process-local structural edits requested by a window-management policy.
/// The tiler validates membership, ownership, cycles and depth before mutation.
#[derive(Event, Clone, Copy, Debug)]
pub enum TileTreeEdit {
    /// Detach a node and insert it beside an existing sibling or ancestor.
    /// Empty source containers are removed; explicit unary splits are retained.
    Place {
        node: Entity,
        anchor: Entity,
        side: TileSide,
    },
    /// Group a container's current children under their existing axis, then
    /// give the outer container a new axis. Child order and weights survive.
    WrapChildren { container: Entity, axis: SplitAxis },
    /// Flatten a unary group containing a split on its parent's axis. The
    /// promoted children retain their combined share and relative proportions.
    Flatten { container: Entity },
}

/// Published after deferred topology edits, before the next queued policy action.
#[derive(Event, Clone, Copy, Debug)]
pub struct TileTreeChanged;

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

/// Marks a shared managed workspace whose layout is owned by this tiler.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct TileWorkspace;

/// Transfer a managed leaf into a workspace's layout. The destination anchor
/// selects insertion after a leaf; absent an anchor, append to the root.
#[derive(Event, Clone, Copy, Debug)]
pub struct TileWorkspaceMove {
    pub window: Entity,
    pub workspace: Entity,
    pub anchor: Option<Entity>,
}

/// Change the selected window's layout mode. `None` toggles the current mode.
#[derive(Event, Clone, Copy, Debug)]
pub struct TileFloatingRequest {
    /// `None` resolves the focused window when the request executes.
    pub window: Option<Entity>,
    pub enabled: Option<bool>,
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
pub struct TileCommands {
    pub(crate) queue: CommandQueue,
    pub(crate) count: usize,
}

/// An ordered edit. Queue with [`bevy::ecs::system::Commands::trigger`] so its
/// topology, geometry and focus effects finish before the next shell action.
#[derive(Event, Clone, Copy, Debug)]
pub enum TileRequest {
    Window(TileCommand),
    Focused(TileOperation),
}

impl TileCommands {
    pub fn push(&mut self, command: TileCommand) -> Result<(), TileCommand> {
        if self.count == COMMAND_CAPACITY {
            return Err(command);
        }
        self.defer(TileRequest::Window(command))
            .map_err(|_| command)
    }

    /// Resolves focus when this operation executes, preserving sequential
    /// keyboard navigation even when several inputs arrive in one frame.
    pub fn push_focused(&mut self, operation: TileOperation) -> Result<(), TileOperation> {
        if self.count == COMMAND_CAPACITY {
            return Err(operation);
        }
        self.defer(TileRequest::Focused(operation))
            .map_err(|_| operation)
    }

    /// Queues a policy event alongside native edits, preserving arrival order
    /// until workspace preparation has completed. The caller retains a rejected
    /// event when the shared bound is reached.
    pub fn defer<'a, E: Event<Trigger<'a>: Default>>(&mut self, event: E) -> Result<(), E> {
        if self.count == COMMAND_CAPACITY {
            return Err(event);
        }
        self.queue.push(command::trigger(event).handle_error());
        self.count += 1;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

#[derive(Resource, Default)]
struct TileState {
    next_id: u64,
}

#[cfg(test)]
mod tests;
