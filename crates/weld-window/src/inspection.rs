//! Window and workspace inventories published by the window-management domain.

use crate::{
    FloatingWindow, FocusedWindow, ManagedWindow, PresentationInsets, PrimaryWindowPresentation,
    StickyWindow, WindowGeometry, WindowId, WindowOccupant, WindowOutput, WindowVisibility,
    fullscreen::{FullscreenMode, FullscreenOccluded, WindowFullscreen},
    workspace::{FocusedWorkspace, Workspace, WorkspaceId, WorkspaceMember, WorkspaceOutput},
};
use bevy::{
    app::{App, Last},
    ecs::{query::QueryData, system::SystemParam},
    prelude::*,
};
use std::collections::BTreeMap;
use weld_app::{
    inspection::{InspectionEnabled, InspectionSystems},
    output::{OutputId, WeldOutput},
    surface::ClientWindowMetadata,
};
use weld_client::ClientSurfaceMetadata;

#[derive(Clone, Debug, PartialEq)]
pub struct WindowSnapshot {
    pub output: Option<OutputId>,
    pub workspace: Option<WorkspaceId>,
    pub geometry: WindowGeometry,
    pub content: WindowGeometry,
    pub occupant: Option<ClientSurfaceMetadata>,
    pub visible: bool,
    pub floating: bool,
    pub sticky: bool,
    pub fullscreen: Option<FullscreenMode>,
}

#[derive(Resource, Default, PartialEq)]
pub struct WindowInventory {
    windows: BTreeMap<WindowId, WindowSnapshot>,
    focused: Option<WindowId>,
}

impl WindowInventory {
    pub fn windows(&self) -> &BTreeMap<WindowId, WindowSnapshot> {
        &self.windows
    }
    pub const fn focused(&self) -> Option<WindowId> {
        self.focused
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSnapshot {
    pub name: String,
    pub output: Option<OutputId>,
    pub visible: bool,
    pub focus_history: Vec<WindowId>,
}

#[derive(Resource, Default, PartialEq)]
pub struct WorkspaceInventory {
    workspaces: BTreeMap<WorkspaceId, WorkspaceSnapshot>,
    focused: Option<WorkspaceId>,
}

impl WorkspaceInventory {
    pub fn workspaces(&self) -> &BTreeMap<WorkspaceId, WorkspaceSnapshot> {
        &self.workspaces
    }
    pub const fn focused(&self) -> Option<WorkspaceId> {
        self.focused
    }
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<WindowInventory>()
        .init_resource::<WorkspaceInventory>()
        .add_systems(
            Last,
            (collect_windows, collect_workspaces)
                .in_set(InspectionSystems::Collect)
                .run_if(resource_exists::<InspectionEnabled>),
        );
}

#[derive(QueryData)]
struct InspectWindow {
    entity: Entity,
    window: &'static ManagedWindow,
    geometry: &'static WindowGeometry,
    visibility: &'static WindowVisibility,
    output: Option<&'static WindowOutput>,
    member: Option<&'static WorkspaceMember>,
    occupant: Option<&'static WindowOccupant>,
    presentation: Option<&'static PrimaryWindowPresentation>,
    fullscreen: Option<&'static WindowFullscreen>,
    floating: Has<FloatingWindow>,
    sticky: Has<StickyWindow>,
    occluded: Has<FullscreenOccluded>,
}

type WindowChanges = Or<(
    Or<(
        Changed<ManagedWindow>,
        Changed<WindowGeometry>,
        Changed<WindowVisibility>,
        Changed<WindowOutput>,
        Changed<WorkspaceMember>,
        Changed<WindowOccupant>,
        Changed<ClientWindowMetadata>,
        Changed<WeldOutput>,
        Changed<Workspace>,
    )>,
    Or<(
        Changed<PresentationInsets>,
        Changed<PrimaryWindowPresentation>,
        Changed<WindowFullscreen>,
        Added<StickyWindow>,
        Added<FullscreenOccluded>,
        Added<FloatingWindow>,
    )>,
)>;

#[derive(SystemParam)]
struct RemovedWindowState<'w, 's> {
    windows: RemovedComponents<'w, 's, ManagedWindow>,
    occupant: RemovedComponents<'w, 's, WindowOccupant>,
    metadata: RemovedComponents<'w, 's, ClientWindowMetadata>,
    presentation: RemovedComponents<'w, 's, PrimaryWindowPresentation>,
    insets: RemovedComponents<'w, 's, PresentationInsets>,
    fullscreen: RemovedComponents<'w, 's, WindowFullscreen>,
    occluded: RemovedComponents<'w, 's, FullscreenOccluded>,
    sticky: RemovedComponents<'w, 's, StickyWindow>,
    floating: RemovedComponents<'w, 's, FloatingWindow>,
    member: RemovedComponents<'w, 's, WorkspaceMember>,
    output: RemovedComponents<'w, 's, WindowOutput>,
    geometry: RemovedComponents<'w, 's, WindowGeometry>,
    visibility: RemovedComponents<'w, 's, WindowVisibility>,
    output_identity: RemovedComponents<'w, 's, WeldOutput>,
    workspace_identity: RemovedComponents<'w, 's, Workspace>,
}

impl RemovedWindowState<'_, '_> {
    fn count(&mut self) -> usize {
        self.windows.read().count()
            + self.occupant.read().count()
            + self.metadata.read().count()
            + self.presentation.read().count()
            + self.insets.read().count()
            + self.fullscreen.read().count()
            + self.occluded.read().count()
            + self.sticky.read().count()
            + self.floating.read().count()
            + self.member.read().count()
            + self.output.read().count()
            + self.geometry.read().count()
            + self.visibility.read().count()
            + self.output_identity.read().count()
            + self.workspace_identity.read().count()
    }
}

#[derive(SystemParam)]
struct WindowSources<'w, 's> {
    windows: Query<'w, 's, InspectWindow>,
    outputs: Query<'w, 's, &'static WeldOutput>,
    workspaces: Query<'w, 's, &'static Workspace>,
    metadata: Query<'w, 's, &'static ClientWindowMetadata>,
    insets: Query<'w, 's, &'static PresentationInsets>,
    focused: Res<'w, FocusedWindow>,
}

fn collect_windows(
    sources: WindowSources,
    changed: Query<(), WindowChanges>,
    mut removed: RemovedWindowState,
    mut published: ResMut<WindowInventory>,
) {
    let removed = removed.count();
    if !published.is_added() && changed.is_empty() && removed == 0 && !sources.focused.is_changed()
    {
        return;
    }
    let mut next = WindowInventory::default();
    for item in &sources.windows {
        let insets = item
            .presentation
            .and_then(|root| sources.insets.get(root.entity()).ok())
            .copied()
            .unwrap_or_default();
        let output = item
            .output
            .and_then(|output| sources.outputs.get(output.0).ok())
            .map(|output| output.id);
        let workspace = item
            .member
            .and_then(|member| sources.workspaces.get(member.0).ok())
            .map(Workspace::id);
        let occupant = item.occupant.map(|occupant| {
            sources
                .metadata
                .get(occupant.entity())
                .map(|metadata| metadata.0.clone())
                .unwrap_or_default()
        });
        if sources.focused.entity() == Some(item.entity) {
            next.focused = Some(item.window.id);
        }
        next.windows.insert(
            item.window.id,
            WindowSnapshot {
                output,
                workspace,
                geometry: *item.geometry,
                content: WindowGeometry {
                    position: Vec2::new(insets.left, insets.top),
                    size: (item.geometry.size - insets.extent()).max(Vec2::ZERO),
                },
                occupant,
                visible: *item.visibility == WindowVisibility::Visible && !item.occluded,
                floating: item.floating,
                sticky: item.sticky,
                fullscreen: item.fullscreen.map(|fullscreen| fullscreen.mode),
            },
        );
    }
    published.set_if_neq(next);
}

type WorkspaceChanges = Or<(
    Changed<Workspace>,
    Changed<WorkspaceOutput>,
    Changed<WeldOutput>,
    Changed<ManagedWindow>,
)>;

fn collect_workspaces(
    workspaces: Query<(Entity, &Workspace, Option<&WorkspaceOutput>)>,
    windows: Query<&ManagedWindow>,
    outputs: Query<&WeldOutput>,
    focus: Res<FocusedWorkspace>,
    changed: Query<(), WorkspaceChanges>,
    mut removed: (
        RemovedComponents<Workspace>,
        RemovedComponents<WorkspaceOutput>,
        RemovedComponents<ManagedWindow>,
        RemovedComponents<WeldOutput>,
    ),
    mut published: ResMut<WorkspaceInventory>,
) {
    let removed = removed.0.read().count()
        + removed.1.read().count()
        + removed.2.read().count()
        + removed.3.read().count();
    if !published.is_added() && changed.is_empty() && removed == 0 && !focus.is_changed() {
        return;
    }
    let mut next = WorkspaceInventory::default();
    for (entity, workspace, output) in &workspaces {
        if focus.entity() == Some(entity) {
            next.focused = Some(workspace.id());
        }
        next.workspaces.insert(
            workspace.id(),
            WorkspaceSnapshot {
                name: workspace.name().to_owned(),
                visible: workspace.visible(),
                output: output
                    .and_then(|output| outputs.get(output.0).ok())
                    .map(|output| output.id),
                focus_history: workspace
                    .recent()
                    .filter_map(|window| windows.get(window).ok().map(|window| window.id))
                    .collect(),
            },
        );
    }
    published.set_if_neq(next);
}
