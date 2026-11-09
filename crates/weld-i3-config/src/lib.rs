//! i3 behavior configuration contracts, without live tree or client state.

use weld_ssd_config::BorderStyle;
use weld_tile_config::{SplitAxis, TileLayout};
use weld_window_config::PersistenceAction;

/// Select an ancestor split or its last-selected direct child.
#[cfg_attr(feature = "ecs", derive(bevy_ecs::event::Event))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum I3FocusHierarchy {
    Parent,
    Child,
}

/// Where directional navigation may wrap after reaching a split edge.
#[cfg_attr(feature = "ecs", derive(bevy_ecs::resource::Resource))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FocusWrapping {
    No,
    #[default]
    Yes,
    Force,
    /// Keeps navigation within the current workspace. Directional output
    /// traversal remains a follow-up, so currently this matches [`Self::Yes`].
    Workspace,
}

/// Workspace selector in i3 command vocabulary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceTarget {
    Name(String),
    Number(String),
    Next,
    Previous,
    NextOnOutput,
    PreviousOnOutput,
    BackAndForth,
    Current,
}

/// Ordered connector preferences for workspace creation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceAssignment {
    pub workspace: String,
    pub outputs: Vec<String>,
}

#[cfg_attr(feature = "ecs", derive(bevy_ecs::resource::Resource))]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceSettings {
    pub assignments: Vec<WorkspaceAssignment>,
    /// Workspace names from switch bindings, in configuration order.
    pub initial_names: Vec<String>,
    /// A named switch/move targeting the selected workspace goes to the previous one.
    pub auto_back_and_forth: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceNavigation {
    Switch,
    MoveWindow,
}

#[cfg_attr(feature = "ecs", derive(bevy_ecs::event::Event))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum I3WorkspaceRequest {
    Navigate {
        target: WorkspaceTarget,
        operation: WorkspaceNavigation,
        allow_auto_back_and_forth: bool,
    },
    Rename {
        source: Option<String>,
        name: String,
    },
}

impl I3WorkspaceRequest {
    pub fn switch(target: WorkspaceTarget) -> Self {
        Self::Navigate {
            target,
            operation: WorkspaceNavigation::Switch,
            allow_auto_back_and_forth: true,
        }
    }
    pub fn move_window(target: WorkspaceTarget) -> Self {
        Self::Navigate {
            target,
            operation: WorkspaceNavigation::MoveWindow,
            allow_auto_back_and_forth: true,
        }
    }
    pub fn without_auto_back_and_forth(mut self) -> Self {
        if let Self::Navigate {
            allow_auto_back_and_forth,
            ..
        } = &mut self
        {
            *allow_auto_back_and_forth = false;
        }
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutChoice {
    Layout(TileLayout),
    /// Restore the last split, or alternate the current split orientation.
    Split,
}

/// Layout vocabulary and cycling belong to the i3 command interpreter.
#[cfg_attr(feature = "ecs", derive(bevy_ecs::event::Event))]
#[derive(Clone, Debug, PartialEq)]
pub enum I3LayoutRequest {
    Set(TileLayout),
    Default,
    Toggle,
    ToggleAll,
    Cycle(Vec<LayoutChoice>),
}

fn opposite(axis: SplitAxis) -> SplitAxis {
    match axis {
        SplitAxis::Horizontal => SplitAxis::Vertical,
        SplitAxis::Vertical => SplitAxis::Horizontal,
    }
}

impl I3LayoutRequest {
    pub fn resolve(&self, current: TileLayout, last_split: SplitAxis) -> TileLayout {
        match self {
            Self::Set(layout) => *layout,
            Self::Default => TileLayout::Split(last_split),
            Self::Toggle => match current {
                TileLayout::Split(_) => TileLayout::Stacked,
                TileLayout::Stacked => TileLayout::Tabbed,
                TileLayout::Tabbed => TileLayout::Split(last_split),
            },
            Self::ToggleAll => match current {
                TileLayout::Split(SplitAxis::Horizontal) => TileLayout::Split(SplitAxis::Vertical),
                TileLayout::Split(SplitAxis::Vertical) => TileLayout::Stacked,
                TileLayout::Stacked => TileLayout::Tabbed,
                TileLayout::Tabbed => TileLayout::Split(SplitAxis::Horizontal),
            },
            Self::Cycle(choices) => {
                let index = choices.iter().position(|choice| match choice {
                    LayoutChoice::Layout(layout) => *layout == current,
                    LayoutChoice::Split => current.is_split(),
                });
                let next = index
                    .and_then(|index| choices.get(index + 1))
                    .or_else(|| choices.first());
                match next {
                    Some(LayoutChoice::Layout(layout)) => *layout,
                    Some(LayoutChoice::Split) => TileLayout::Split(if current.is_split() {
                        opposite(last_split)
                    } else {
                        last_split
                    }),
                    None => current,
                }
            }
        }
    }
}

#[cfg_attr(feature = "ecs", derive(bevy_ecs::event::Event))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct I3ResizeRequest {
    pub axis: SplitAxis,
    pub pixels: Option<f32>,
    pub fraction: Option<f32>,
}

#[derive(Clone, Debug)]
pub enum RuleEffect {
    Commands(Vec<RuleAction>),
    Assign(WorkspaceTarget),
    NoFocus,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RuleAction {
    Persistent(PersistenceAction),
    Border(BorderStyle),
    Floating(bool),
    Workspace(WorkspaceTarget),
    Position([f32; 2]),
}

#[derive(Clone, Copy, Debug)]
pub enum Field {
    AppId,
    Class,
    Instance,
    Title,
}

/// Native rule intent; regex construction is deferred until preparation.
#[derive(Clone, Debug)]
pub struct RuleDefinition {
    pub criteria: Vec<(Field, String)>,
    pub effect: RuleEffect,
}
