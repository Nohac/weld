//! Projects durable workspace state into desktop protocol inventory.

use crate::{
    WindowSystems,
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
        system::{Commands, Query, ResMut},
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

fn publish(
    workspaces: Query<(&Workspace, Option<&WorkspaceOutput>)>,
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
    if !published.is_added()
        && changed_workspaces.is_empty()
        && changed_outputs.is_empty()
        && removed_count == 0
    {
        return;
    }
    published.publish(
        workspaces
            .iter()
            .map(|(workspace, output)| DesktopWorkspace {
                id: DesktopWorkspaceId::new(workspace.id().raw()),
                name: workspace.name().to_owned(),
                output: output
                    .and_then(|output| outputs.get(output.0).ok())
                    .map(|output| output.id),
                active: workspace.visible(),
            })
            .collect(),
    );
}
