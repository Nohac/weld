//! Durable workspace membership, output attachment and local presentation.

use bevy::{
    ecs::{
        component::Component,
        entity::Entity,
        event::Event,
        lifecycle::Remove,
        message::MessageWriter,
        observer::On,
        query::With,
        resource::Resource,
        system::{Commands, Query, Res, ResMut, SystemParam},
    },
    window::RequestRedraw,
};
use std::collections::HashSet;
use weld_app::output::WeldOutput;

use crate::{
    FocusedWindow, ManagedWindow, WindowCommand, WindowCommandKind, WindowFocusChanged,
    WindowOutput, WindowVisibility,
};

/// Session-stable identity suitable for configuration and external references.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceId(u64);

impl WorkspaceId {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// A durable collection of managed windows with its own layout and selection.
#[derive(Component, Debug)]
pub struct Workspace {
    id: WorkspaceId,
    name: String,
    visible: bool,
    history: Vec<Entity>,
}

impl Workspace {
    pub const fn id(&self) -> WorkspaceId {
        self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub const fn visible(&self) -> bool {
        self.visible
    }
    pub fn recent(&self) -> impl Iterator<Item = Entity> + '_ {
        self.history.iter().copied()
    }
}

/// Workspace membership survives client unmapping, hoisting and vacancy.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[relationship(relationship_target = WorkspaceWindows)]
pub struct WorkspaceMember(pub Entity);

/// Managed windows belonging to a workspace. Members have independent lifetimes.
#[derive(Component, Debug)]
#[relationship_target(relationship = WorkspaceMember)]
pub struct WorkspaceWindows(Vec<Entity>);

impl WorkspaceWindows {
    pub fn iter(&self) -> impl Iterator<Item = Entity> + '_ {
        self.0.iter().copied()
    }
}

/// The output in whose local logical coordinates this workspace is laid out.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[relationship(relationship_target = OutputWorkspaces)]
pub struct WorkspaceOutput(pub Entity);

/// Workspaces assigned to this output; visibility is chosen separately by policy.
#[derive(Component, Debug)]
#[relationship_target(relationship = WorkspaceOutput)]
pub struct OutputWorkspaces(Vec<Entity>);

impl OutputWorkspaces {
    pub fn iter(&self) -> impl Iterator<Item = Entity> + '_ {
        self.0.iter().copied()
    }
}

/// Current management context, including when the selected workspace is empty.
#[derive(Resource, Default, Debug)]
pub struct FocusedWorkspace(Option<Entity>);

impl FocusedWorkspace {
    pub const fn entity(&self) -> Option<Entity> {
        self.0
    }
}

#[derive(Resource, Default)]
pub(crate) struct WorkspaceIds {
    next: u64,
    names: HashSet<String>,
}

/// Validated workspace allocation. Layout plugins initialize on [`WorkspaceCreated`].
#[derive(SystemParam)]
pub struct WorkspaceCreation<'w, 's> {
    ids: ResMut<'w, WorkspaceIds>,
    outputs: Query<'w, 's, (), With<WeldOutput>>,
    commands: Commands<'w, 's>,
}

impl WorkspaceCreation<'_, '_> {
    pub fn create(&mut self, name: String, output: Entity) -> Option<Entity> {
        if name.is_empty() || !self.outputs.contains(output) || self.ids.names.contains(&name) {
            return None;
        }
        let next = self.ids.next.checked_add(1)?;
        let id = WorkspaceId(self.ids.next);
        self.ids.next = next;
        self.ids.names.insert(name.clone());
        let workspace = self
            .commands
            .spawn((
                Workspace {
                    id,
                    name,
                    visible: false,
                    history: Vec::new(),
                },
                WorkspaceOutput(output),
            ))
            .id();
        self.commands.trigger(WorkspaceCreated(workspace));
        Some(workspace)
    }
}

/// Published after the new workspace and output relationship exist.
#[derive(Event, Clone, Copy, Debug)]
pub struct WorkspaceCreated(pub Entity);

/// Native workspace operations. Policies choose which peers to hide or display.
#[derive(Event, Clone, Copy, Debug)]
pub enum WorkspaceRequest {
    /// Reassign coordinates/output membership while retaining visibility.
    /// A single-workspace presenter hides the old presentation before assigning.
    Assign {
        workspace: Entity,
        output: Entity,
    },
    SetVisible {
        workspace: Entity,
        visible: bool,
    },
    Focus(Entity),
    /// Select a policy-chosen member, or the empty workspace itself.
    Select {
        workspace: Entity,
        window: Option<Entity>,
    },
    RemoveEmpty(Entity),
}

#[derive(Event, Clone, Copy, Debug)]
pub struct WorkspaceFocused {
    pub previous: Option<Entity>,
    pub workspace: Entity,
}

pub(crate) fn removed(
    event: On<Remove, Workspace>,
    workspaces: Query<&Workspace>,
    mut ids: ResMut<WorkspaceIds>,
    mut selected: ResMut<FocusedWorkspace>,
) {
    if let Ok(workspace) = workspaces.get(event.entity) {
        ids.names.remove(workspace.name());
    }
    if selected.0 == Some(event.entity) {
        selected.0 = None;
    }
}

/// Updates remembered selection after a layout commits a membership transfer.
#[derive(Event, Clone, Copy, Debug)]
pub struct WorkspaceMemberMoved {
    pub window: Entity,
    pub previous: Entity,
}

pub(crate) fn member_moved(
    event: On<WorkspaceMemberMoved>,
    members: Query<&WorkspaceMember>,
    mut workspaces: Query<&mut Workspace>,
) {
    let Ok(member) = members.get(event.window) else {
        return;
    };
    if member.0 == event.previous {
        return;
    }
    if let Ok(mut old) = workspaces.get_mut(event.previous) {
        old.history.retain(|window| *window != event.window);
    }
    if let Ok(mut new) = workspaces.get_mut(member.0) {
        new.history.retain(|window| *window != event.window);
        new.history.insert(0, event.window);
    }
}

#[derive(SystemParam)]
pub(crate) struct WorkspaceAccess<'w, 's> {
    workspaces: Query<'w, 's, (&'static mut Workspace, Option<&'static WorkspaceOutput>)>,
    members: Query<'w, 's, (Entity, &'static ManagedWindow, &'static WorkspaceMember)>,
    outputs: Query<'w, 's, (), With<WeldOutput>>,
    selected: ResMut<'w, FocusedWorkspace>,
    focus: Res<'w, FocusedWindow>,
    commands: Commands<'w, 's>,
    redraw: MessageWriter<'w, RequestRedraw>,
}

pub(crate) fn request(event: On<WorkspaceRequest>, mut access: WorkspaceAccess) {
    match *event.event() {
        WorkspaceRequest::Assign { workspace, output } => {
            if !access.workspaces.contains(workspace) || !access.outputs.contains(output) {
                return;
            }
            access
                .commands
                .entity(workspace)
                .insert(WorkspaceOutput(output));
            for (window, _, member) in &access.members {
                if member.0 == workspace {
                    access.commands.entity(window).insert(WindowOutput(output));
                }
            }
        }
        WorkspaceRequest::SetVisible { workspace, visible } => {
            let Ok((mut state, output)) = access.workspaces.get_mut(workspace) else {
                return;
            };
            if visible && output.is_none_or(|output| !access.outputs.contains(output.0)) {
                return;
            }
            if state.visible == visible {
                return;
            }
            state.visible = visible;
            for (window, _, member) in &access.members {
                if member.0 == workspace {
                    access.commands.entity(window).insert(if visible {
                        WindowVisibility::Visible
                    } else {
                        WindowVisibility::Hidden
                    });
                }
            }
        }
        WorkspaceRequest::Focus(workspace) | WorkspaceRequest::Select { workspace, .. } => {
            let Ok((state, output)) = access.workspaces.get(workspace) else {
                return;
            };
            if !state.visible || output.is_none_or(|output| !access.outputs.contains(output.0)) {
                return;
            }
            let selected = if let WorkspaceRequest::Select { window, .. } = *event.event() {
                if window.is_some_and(|window| {
                    !access
                        .members
                        .get(window)
                        .is_ok_and(|(_, _, member)| member.0 == workspace)
                }) {
                    return;
                }
                window
            } else {
                state
                    .recent()
                    .find(|window| {
                        access
                            .members
                            .get(*window)
                            .is_ok_and(|(_, _, member)| member.0 == workspace)
                    })
                    .or_else(|| {
                        access
                            .members
                            .iter()
                            .filter(|(_, _, member)| member.0 == workspace)
                            .min_by_key(|(_, window, _)| window.id)
                            .map(|(entity, _, _)| entity)
                    })
            };
            let previous = access.selected.0.replace(workspace);
            if previous != Some(workspace) {
                access.commands.trigger(WorkspaceFocused {
                    previous,
                    workspace,
                });
            }
            if let Some(window) = selected {
                access.commands.trigger(WindowCommand {
                    window,
                    kind: WindowCommandKind::Focus,
                });
            } else if let Some(window) = access.focus.entity() {
                access.commands.trigger(WindowCommand {
                    window,
                    kind: WindowCommandKind::ClearFocus,
                });
            }
        }
        WorkspaceRequest::RemoveEmpty(workspace) => {
            let Ok((state, _)) = access.workspaces.get(workspace) else {
                return;
            };
            if state.visible
                || access.selected.0 == Some(workspace)
                || access
                    .members
                    .iter()
                    .any(|(_, _, member)| member.0 == workspace)
            {
                return;
            }
            access.commands.entity(workspace).despawn();
        }
    }
    access.redraw.write(RequestRedraw);
}

pub(crate) fn remember_focus(
    event: On<WindowFocusChanged>,
    members: Query<&WorkspaceMember, With<ManagedWindow>>,
    mut workspaces: Query<&mut Workspace>,
    mut selected: ResMut<FocusedWorkspace>,
    mut commands: Commands,
) {
    let Some(window) = event.window else { return };
    let Ok(member) = members.get(window) else {
        return;
    };
    let Ok(mut workspace) = workspaces.get_mut(member.0) else {
        return;
    };
    workspace.history.retain(|entry| {
        *entry != window && members.get(*entry).is_ok_and(|other| other.0 == member.0)
    });
    workspace.history.insert(0, window);
    if workspace.visible {
        let previous = selected.0.replace(member.0);
        if previous != Some(member.0) {
            commands.trigger(WorkspaceFocused {
                previous,
                workspace: member.0,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WindowId, WindowPlugin};
    use bevy::{app::App, ecs::system::RunSystemOnce};
    use weld_app::output::OutputId;

    fn setup() -> (App, Entity, Entity, Entity) {
        let mut app = App::new();
        app.add_plugins(WindowPlugin);
        let output = app
            .world_mut()
            .spawn(WeldOutput {
                id: OutputId::new(1),
            })
            .id();
        let (first, second) = app
            .world_mut()
            .run_system_once(move |mut creation: WorkspaceCreation| {
                (
                    creation.create("first".to_owned(), output).expect("first"),
                    creation
                        .create("second".to_owned(), output)
                        .expect("second"),
                )
            })
            .expect("allocation");
        (app, output, first, second)
    }

    fn request(app: &mut App, request: WorkspaceRequest) {
        app.world_mut().trigger(request);
        app.world_mut().flush();
    }

    #[test]
    fn removal_requires_hidden_unselected_and_empty_workspace() {
        let (mut app, _, first, second) = setup();
        request(
            &mut app,
            WorkspaceRequest::SetVisible {
                workspace: first,
                visible: true,
            },
        );
        request(&mut app, WorkspaceRequest::RemoveEmpty(first));
        assert!(app.world().get::<Workspace>(first).is_some());
        request(&mut app, WorkspaceRequest::Focus(first));
        request(
            &mut app,
            WorkspaceRequest::SetVisible {
                workspace: first,
                visible: false,
            },
        );
        request(&mut app, WorkspaceRequest::RemoveEmpty(first));
        assert!(app.world().get::<Workspace>(first).is_some());
        let window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(1),
                },
                WorkspaceMember(second),
            ))
            .id();
        request(&mut app, WorkspaceRequest::RemoveEmpty(second));
        assert!(app.world().get::<Workspace>(second).is_some());
        app.world_mut().despawn(window);
        request(&mut app, WorkspaceRequest::RemoveEmpty(second));
        assert!(app.world().get::<Workspace>(second).is_none());
    }

    #[test]
    fn foreign_selection_and_unavailable_output_requests_are_rejected() {
        let (mut app, output, first, second) = setup();
        let foreign = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(1),
                },
                WorkspaceMember(second),
            ))
            .id();
        request(
            &mut app,
            WorkspaceRequest::SetVisible {
                workspace: first,
                visible: true,
            },
        );
        request(
            &mut app,
            WorkspaceRequest::Select {
                workspace: first,
                window: Some(foreign),
            },
        );
        assert!(
            app.world()
                .resource::<FocusedWorkspace>()
                .entity()
                .is_none()
        );
        request(
            &mut app,
            WorkspaceRequest::SetVisible {
                workspace: first,
                visible: false,
            },
        );
        app.world_mut().despawn(output);
        request(
            &mut app,
            WorkspaceRequest::SetVisible {
                workspace: first,
                visible: true,
            },
        );
        request(&mut app, WorkspaceRequest::Focus(first));
        assert!(
            !app.world()
                .get::<Workspace>(first)
                .expect("retained")
                .visible()
        );
        assert!(
            app.world()
                .resource::<FocusedWorkspace>()
                .entity()
                .is_none()
        );
    }

    #[test]
    fn names_are_reserved_before_deferred_publication_and_released_on_removal() {
        let (mut app, output, _, _) = setup();
        let new = app
            .world_mut()
            .run_system_once(move |mut creation: WorkspaceCreation| {
                let new = creation.create("new".to_owned(), output).expect("new");
                assert!(creation.create("new".to_owned(), output).is_none());
                new
            })
            .expect("creation");
        request(&mut app, WorkspaceRequest::RemoveEmpty(new));
        assert!(
            app.world_mut()
                .run_system_once(move |mut creation: WorkspaceCreation| creation
                    .create("new".to_owned(), output))
                .expect("recreate")
                .is_some()
        );
    }
}
