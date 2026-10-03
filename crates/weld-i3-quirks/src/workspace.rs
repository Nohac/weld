//! i3 workspace naming, monitor selection and switch/move policy.

use crate::tree::TreeView;
use bevy::ecs::{
    entity::Entity,
    event::Event,
    observer::On,
    resource::Resource,
    system::{Commands, Query, Res, ResMut, SystemParam},
};
use weld_app::output::{OutputInfo, PrimaryOutput, WeldOutput};
use weld_tile::{TileCommands, TileWorkspaceMove};
use weld_window::workspace::{
    FocusedWorkspace, Workspace, WorkspaceCreation, WorkspaceFocused, WorkspaceMember,
    WorkspaceOutput, WorkspaceRequest,
};
use weld_window::workspace_protocol::ActivateWorkspace;
use weld_window::{FocusedWindow, ManagedWindow};

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

#[derive(Resource, Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceSettings {
    pub assignments: Vec<WorkspaceAssignment>,
    /// Workspace names from switch bindings, in configuration order.
    pub initial_names: Vec<String>,
}

#[derive(Event, Clone, Debug, Eq, PartialEq)]
pub enum I3WorkspaceRequest {
    Switch(WorkspaceTarget),
    MoveWindow(WorkspaceTarget),
}

#[derive(Resource, Default)]
pub(crate) struct PreviousWorkspace(Option<String>);

#[derive(Event)]
pub(crate) enum Resolved {
    Switch(Entity),
    Move { window: Entity, workspace: Entity },
}

#[derive(Event)]
pub(crate) struct AfterMove {
    window: Entity,
    source: Entity,
    destination: Entity,
    ancestors: Vec<Entity>,
}

#[derive(SystemParam)]
pub(crate) struct WorkspaceView<'w, 's> {
    workspaces: Query<'w, 's, (Entity, &'static Workspace, Option<&'static WorkspaceOutput>)>,
    outputs: Query<
        'w,
        's,
        (
            Entity,
            &'static WeldOutput,
            Option<&'static OutputInfo>,
            Option<&'static PrimaryOutput>,
        ),
    >,
    selected: Res<'w, FocusedWorkspace>,
    settings: Res<'w, WorkspaceSettings>,
    previous: Res<'w, PreviousWorkspace>,
}

/// i3 recognizes a leading decimal number, including numbered descriptive names.
pub(crate) fn number(name: &str) -> Option<u32> {
    let digits = name.bytes().take_while(u8::is_ascii_digit).count();
    name.get(..digits)?
        .parse::<i32>()
        .ok()
        .and_then(|number| u32::try_from(number).ok())
}

impl WorkspaceView<'_, '_> {
    fn current_output(&self) -> Option<Entity> {
        self.selected
            .entity()
            .and_then(|selected| self.workspaces.get(selected).ok())
            .and_then(|(_, _, output)| output)
            .map(|output| output.0)
            .filter(|output| self.outputs.contains(*output))
            .or_else(|| {
                self.outputs
                    .iter()
                    .filter(|(_, _, _, primary)| primary.is_some())
                    .min_by_key(|(_, output, _, _)| output.id)
                    .map(|(entity, _, _, _)| entity)
            })
            .or_else(|| {
                self.outputs
                    .iter()
                    .min_by_key(|(_, output, _, _)| output.id)
                    .map(|(entity, _, _, _)| entity)
            })
    }

    fn preferred_output(&self, name: &str) -> Option<Entity> {
        let resolve = |assignment: &WorkspaceAssignment| {
            assignment.outputs.iter().find_map(|wanted| {
                self.outputs
                    .iter()
                    .filter(|(_, _, info, primary)| {
                        if wanted == "primary" {
                            primary.is_some()
                        } else if wanted == "nonprimary" {
                            primary.is_none()
                        } else {
                            info.is_some_and(|info| info.name() == wanted)
                        }
                    })
                    .min_by_key(|(_, output, _, _)| output.id)
                    .map(|(entity, _, _, _)| entity)
            })
        };
        self.settings
            .assignments
            .iter()
            .filter(|assignment| assignment.workspace == name)
            .find_map(resolve)
            .or_else(|| {
                self.settings
                    .assignments
                    .iter()
                    .filter(|assignment| {
                        assignment
                            .workspace
                            .bytes()
                            .all(|byte| byte.is_ascii_digit())
                            && number(name)
                                .is_some_and(|n| number(&assignment.workspace) == Some(n))
                    })
                    .find_map(resolve)
            })
    }

    fn find(&self, target: &WorkspaceTarget) -> Option<Entity> {
        match target {
            WorkspaceTarget::Name(name) => self
                .workspaces
                .iter()
                .filter(|(_, workspace, _)| workspace.name().eq_ignore_ascii_case(name))
                .min_by_key(|(_, workspace, _)| workspace.id())
                .map(|(entity, _, _)| entity),
            WorkspaceTarget::Number(name) => self
                .workspaces
                .iter()
                .filter(|(_, workspace, _)| number(workspace.name()) == number(name))
                .min_by_key(|(_, workspace, _)| workspace.id())
                .map(|(entity, _, _)| entity),
            WorkspaceTarget::Current => self.selected.entity(),
            WorkspaceTarget::BackAndForth => self
                .previous
                .0
                .as_ref()
                .and_then(|name| self.find(&WorkspaceTarget::Name(name.clone()))),
            WorkspaceTarget::Next
            | WorkspaceTarget::Previous
            | WorkspaceTarget::NextOnOutput
            | WorkspaceTarget::PreviousOnOutput => {
                let same_output = matches!(
                    target,
                    WorkspaceTarget::NextOnOutput | WorkspaceTarget::PreviousOnOutput
                );
                let current_output = self.current_output();
                let mut ordered: Vec<_> = self
                    .workspaces
                    .iter()
                    .filter(|(_, _, output)| {
                        !same_output
                            || output.is_some_and(|output| Some(output.0) == current_output)
                    })
                    .collect();
                ordered.sort_by_key(|(_, workspace, output)| {
                    (
                        number(workspace.name()).is_none(),
                        number(workspace.name()),
                        output
                            .and_then(|output| self.outputs.get(output.0).ok())
                            .map(|(_, output, _, _)| output.id),
                        workspace.id(),
                    )
                });
                let index = ordered
                    .iter()
                    .position(|(entity, _, _)| Some(*entity) == self.selected.entity())?;
                let next = if matches!(
                    target,
                    WorkspaceTarget::Next | WorkspaceTarget::NextOnOutput
                ) {
                    (index + 1) % ordered.len()
                } else {
                    (index + ordered.len() - 1) % ordered.len()
                };
                ordered.get(next).map(|(entity, _, _)| *entity)
            }
        }
    }

    fn creation_name(&self, target: &WorkspaceTarget) -> Option<String> {
        match target {
            WorkspaceTarget::Name(name) | WorkspaceTarget::Number(name) => Some(name.clone()),
            WorkspaceTarget::BackAndForth => self.previous.0.clone(),
            _ => None,
        }
    }
}

pub(crate) fn activate_existing(event: On<ActivateWorkspace>, mut commands: Commands) {
    commands.trigger(Resolved::Switch(event.0));
}

pub(crate) fn request(
    event: On<I3WorkspaceRequest>,
    view: WorkspaceView,
    focus: Res<FocusedWindow>,
    mut creation: WorkspaceCreation,
    mut pending: ResMut<TileCommands>,
    mut commands: Commands,
) {
    if view.current_output().is_none() {
        let _ = pending.defer(event.event().clone());
        return;
    }
    let target = match event.event() {
        I3WorkspaceRequest::Switch(target) | I3WorkspaceRequest::MoveWindow(target) => target,
    };
    if matches!(event.event(), I3WorkspaceRequest::MoveWindow(_)) && focus.entity().is_none() {
        return;
    }
    let workspace = view.find(target).or_else(|| {
        let name = view.creation_name(target)?;
        let output = view
            .preferred_output(&name)
            .or_else(|| view.current_output())?;
        creation.create(name, output)
    });
    let Some(workspace) = workspace else { return };
    match event.event() {
        I3WorkspaceRequest::Switch(_) => commands.trigger(Resolved::Switch(workspace)),
        I3WorkspaceRequest::MoveWindow(_) => {
            if let Some(window) = focus.entity() {
                commands.trigger(Resolved::Move { window, workspace });
            }
        }
    }
}

pub(crate) fn apply_resolved(
    event: On<Resolved>,
    view: WorkspaceView,
    tree: TreeView,
    members: Query<&WorkspaceMember>,
    mut commands: Commands,
) {
    match *event.event() {
        Resolved::Switch(workspace) => {
            let Ok((_, _, Some(output))) = view.workspaces.get(workspace) else {
                return;
            };
            for (other, state, assigned) in &view.workspaces {
                if other != workspace
                    && state.visible()
                    && assigned.is_some_and(|assigned| assigned.0 == output.0)
                {
                    commands.trigger(WorkspaceRequest::SetVisible {
                        workspace: other,
                        visible: false,
                    });
                }
            }
            commands.trigger(WorkspaceRequest::SetVisible {
                workspace,
                visible: true,
            });
            commands.trigger(WorkspaceRequest::Select {
                workspace,
                window: tree.descend(workspace),
            });
        }
        Resolved::Move { window, workspace } => {
            if !view.workspaces.contains(workspace) {
                return;
            }
            let Ok(source) = members.get(window) else {
                return;
            };
            if source.0 == workspace {
                return;
            }
            let anchor = tree.descend(workspace);
            let mut ancestors = Vec::new();
            let mut node = window;
            while let Ok(parent) = tree.parents.get(node) {
                node = parent.entity();
                ancestors.push(node);
            }
            commands.trigger(TileWorkspaceMove {
                window,
                workspace,
                anchor,
            });
            commands.trigger(AfterMove {
                window,
                source: source.0,
                destination: workspace,
                ancestors,
            });
        }
    }
}

pub(crate) fn after_move(
    event: On<AfterMove>,
    tree: TreeView,
    members: Query<&WorkspaceMember>,
    mut commands: Commands,
) {
    if !members
        .get(event.window)
        .is_ok_and(|member| member.0 == event.destination)
    {
        return;
    }
    let window = event
        .ancestors
        .iter()
        .find_map(|ancestor| tree.descend(*ancestor));
    commands.trigger(WorkspaceRequest::Select {
        workspace: event.source,
        window,
    });
}

pub(crate) fn remember(
    event: On<WorkspaceFocused>,
    workspaces: Query<&Workspace>,
    mut previous: ResMut<PreviousWorkspace>,
) {
    if let Some(old) = event
        .previous
        .and_then(|entity| workspaces.get(entity).ok())
    {
        previous.0 = Some(old.name().to_owned());
    }
}

pub(crate) fn prepare(view: WorkspaceView, mut commands: Commands) {
    let mut outputs: Vec<_> = view.outputs.iter().collect();
    outputs.sort_by_key(|(_, output, _, primary)| (primary.is_none(), output.id));
    // Preserve orphaned workspaces and their trees when their output disappears.
    if let Some(fallback) = view.current_output() {
        for (workspace, _, output) in &view.workspaces {
            if output.is_none_or(|output| !view.outputs.contains(output.0)) {
                commands.trigger(WorkspaceRequest::SetVisible {
                    workspace,
                    visible: false,
                });
                commands.trigger(WorkspaceRequest::Assign {
                    workspace,
                    output: fallback,
                });
                if view.selected.entity() == Some(workspace) {
                    commands.trigger(Resolved::Switch(workspace));
                }
            }
        }
    }
    for (output, _, _, _) in outputs {
        if view.selected.entity().is_none()
            || !view.workspaces.iter().any(|(_, state, assigned)| {
                state.visible() && assigned.is_some_and(|assigned| assigned.0 == output)
            })
        {
            commands.trigger(BootstrapOutput(output));
        }
    }
}

#[derive(Event)]
pub(crate) struct BootstrapOutput(Entity);

pub(crate) fn bootstrap(
    event: On<BootstrapOutput>,
    view: WorkspaceView,
    mut creation: WorkspaceCreation,
    mut commands: Commands,
) {
    let output = event.0;
    if let Some((workspace, _, _)) = view.workspaces.iter().find(|(_, state, assigned)| {
        state.visible() && assigned.is_some_and(|assigned| assigned.0 == output)
    }) {
        if view.selected.entity().is_none() {
            commands.trigger(Resolved::Switch(workspace));
        }
        return;
    }
    let existing = view
        .workspaces
        .iter()
        .filter(|(_, _, assigned)| assigned.is_some_and(|assigned| assigned.0 == output))
        .min_by_key(|(_, workspace, _)| workspace.id())
        .map(|(entity, _, _)| entity);
    let workspace = existing.or_else(|| {
        let unused = |name: &str| {
            !view.workspaces.iter().any(|(_, workspace, _)| {
                workspace.name().eq_ignore_ascii_case(name)
                    || number(name).is_some_and(|n| number(workspace.name()) == Some(n))
            })
        };
        let name = view
            .settings
            .assignments
            .iter()
            .filter(|assignment| view.preferred_output(&assignment.workspace) == Some(output))
            .map(|assignment| &assignment.workspace)
            .chain(view.settings.initial_names.iter())
            .find(|name| {
                unused(name)
                    && view
                        .preferred_output(name)
                        .is_none_or(|preferred| preferred == output)
            })
            .cloned()
            .or_else(|| {
                (1..=view.workspaces.iter().count() + view.settings.assignments.len() + 1)
                    .map(|n| n.to_string())
                    .find(|name| {
                        unused(name)
                            && view
                                .preferred_output(name)
                                .is_none_or(|preferred| preferred == output)
                    })
            })?;
        creation.create(name, output)
    });
    if let Some(workspace) = workspace {
        commands.trigger(WorkspaceRequest::SetVisible {
            workspace,
            visible: true,
        });
        if view
            .selected
            .entity()
            .is_none_or(|entity| !view.workspaces.contains(entity))
        {
            commands.trigger(WorkspaceRequest::Focus(workspace));
        }
    }
}

pub(crate) fn reap(
    view: WorkspaceView,
    windows: Query<&WorkspaceMember, bevy::ecs::query::With<ManagedWindow>>,
    mut commands: Commands,
) {
    for (workspace, state, _) in &view.workspaces {
        if !state.visible()
            && view.selected.entity() != Some(workspace)
            && !windows.iter().any(|member| member.0 == workspace)
        {
            commands.trigger(WorkspaceRequest::RemoveEmpty(workspace));
        }
    }
}
