//! Projects durable workspace state into desktop protocol inventory.

use crate::{
    WindowSystems,
    attention::WindowAttention,
    workspace::WorkspaceMember,
    workspace::{Workspace, WorkspaceOutput},
};
use bevy::{
    app::{App, Plugin, PostUpdate, PreUpdate},
    ecs::{
        change_detection::DetectChanges,
        entity::Entity,
        event::Event,
        lifecycle::RemovedComponents,
        message::MessageReader,
        query::{Changed, Or},
        schedule::IntoScheduleConfigs,
        system::{Commands, Query, ResMut, SystemParam},
    },
};
use weld_app::{
    output::WeldOutput,
    workspace::{
        DesktopWorkspace, DesktopWorkspaceActivation, DesktopWorkspaceId, DesktopWorkspaces,
    },
};

/// Requests policy-specific activation of an existing workspace.
#[derive(Event, Clone, Copy, Debug)]
pub struct ActivateWorkspace(pub Entity);

/// Enables local workspace discovery for a WM handling [`ActivateWorkspace`].
pub struct WorkspaceProtocolPlugin;

type WorkspaceChanged = Or<(Changed<Workspace>, Changed<WorkspaceOutput>)>;

impl Plugin for WorkspaceProtocolPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DesktopWorkspaces>()
            .add_message::<DesktopWorkspaceActivation>()
            .add_systems(PreUpdate, activate.before(WindowSystems::Management))
            .add_systems(PostUpdate, publish);
    }
}

fn activate(
    mut requests: MessageReader<DesktopWorkspaceActivation>,
    workspaces: Query<(Entity, &Workspace)>,
    mut commands: Commands,
) {
    for transaction in requests.read() {
        for id in &transaction.0 {
            if let Some((entity, _)) = workspaces
                .iter()
                .find(|(_, workspace)| workspace.id().raw() == id.raw())
            {
                commands.trigger(ActivateWorkspace(entity));
            }
        }
    }
}

#[derive(SystemParam)]
struct WorkspaceAttention<'w, 's> {
    windows: Query<'w, 's, (&'static WorkspaceMember, &'static WindowAttention)>,
    changed: Query<'w, 's, (), AttentionChanges>,
    removed: (
        RemovedComponents<'w, 's, WindowAttention>,
        RemovedComponents<'w, 's, WorkspaceMember>,
    ),
}

type AttentionChanges = Or<(Changed<WorkspaceMember>, Changed<WindowAttention>)>;

impl WorkspaceAttention<'_, '_> {
    fn changed(&mut self) -> bool {
        let removed = self.removed.0.read().count() + self.removed.1.read().count();
        removed != 0 || !self.changed.is_empty()
    }
    fn urgent(&self, workspace: Entity) -> bool {
        self.windows
            .iter()
            .any(|(member, attention)| member.0 == workspace && attention.is_urgent())
    }
}

fn publish(
    workspaces: Query<(Entity, &Workspace, Option<&WorkspaceOutput>)>,
    mut attention: WorkspaceAttention,
    outputs: Query<&WeldOutput>,
    changed_workspaces: Query<(), WorkspaceChanged>,
    changed_outputs: Query<(), Changed<WeldOutput>>,
    mut removed: (
        RemovedComponents<Workspace>,
        RemovedComponents<WorkspaceOutput>,
        RemovedComponents<WeldOutput>,
    ),
    mut published: ResMut<DesktopWorkspaces>,
) {
    let removed_count =
        removed.0.read().count() + removed.1.read().count() + removed.2.read().count();
    let attention_changed = attention.changed();
    if !published.is_added()
        && changed_workspaces.is_empty()
        && changed_outputs.is_empty()
        && !attention_changed
        && removed_count == 0
    {
        return;
    }
    published.publish(
        workspaces
            .iter()
            .map(|(entity, workspace, output)| DesktopWorkspace {
                id: DesktopWorkspaceId::new(workspace.id().raw()),
                name: workspace.name().to_owned(),
                output: output
                    .and_then(|output| outputs.get(output.0).ok())
                    .map(|output| output.id),
                active: workspace.visible(),
                urgent: attention.urgent(entity),
            })
            .collect(),
    );
}
