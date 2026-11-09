//! Workspace selection and navigation contracts.

/// Workspace selector including named, numbered and history-based selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceTarget {
    Name(String),
    /// Match a leading workspace number, retaining the full name for creation.
    NumberedName(String),
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
pub enum WorkspaceCommand {
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

impl WorkspaceCommand {
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
