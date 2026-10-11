//! Stable-ID tiling topology and selection for desktop inspection consumers.

use crate::{
    ContainerId, TileContainer, TileFocusHistory, TileGeometry, TileLayout, TileSelection,
};
use bevy::{
    app::{App, Last},
    ecs::system::SystemParam,
    prelude::*,
};
use std::collections::BTreeMap;
use weld_app::inspection::{InspectionEnabled, InspectionSystems};
use weld_window::{
    ManagedWindow, WindowGeometry, WindowId,
    workspace::{Workspace, WorkspaceId},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum TileNodeId {
    Window(WindowId),
    Workspace(WorkspaceId),
    Container(ContainerId),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TileSnapshot {
    pub geometry: WindowGeometry,
    pub layout: TileLayout,
    pub children: Vec<TileNodeId>,
    pub focus_history: Vec<TileNodeId>,
}

#[derive(Resource, Default, PartialEq)]
pub struct TileInventory {
    containers: BTreeMap<TileNodeId, TileSnapshot>,
    selected: Option<TileNodeId>,
}

impl TileInventory {
    pub fn containers(&self) -> &BTreeMap<TileNodeId, TileSnapshot> {
        &self.containers
    }
    pub const fn selected(&self) -> Option<TileNodeId> {
        self.selected
    }
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<TileInventory>().add_systems(
        Last,
        collect
            .in_set(InspectionSystems::Collect)
            .run_if(resource_exists::<InspectionEnabled>),
    );
}

#[derive(SystemParam)]
struct NodeIdentities<'w, 's> {
    windows: Query<'w, 's, &'static ManagedWindow>,
    workspaces: Query<'w, 's, &'static Workspace>,
    containers: Query<'w, 's, &'static TileContainer>,
}

impl NodeIdentities<'_, '_> {
    fn resolve(&self, entity: Entity) -> Option<TileNodeId> {
        if let Ok(window) = self.windows.get(entity) {
            Some(TileNodeId::Window(window.id))
        } else if let Ok(workspace) = self.workspaces.get(entity) {
            Some(TileNodeId::Workspace(workspace.id()))
        } else {
            self.containers
                .get(entity)
                .ok()
                .map(|container| TileNodeId::Container(container.id()))
        }
    }
}

type TileChanges = Or<(
    Changed<TileContainer>,
    Changed<TileGeometry>,
    Changed<Workspace>,
    Changed<ManagedWindow>,
)>;

fn collect(
    containers: Query<(Entity, &TileContainer, &TileGeometry)>,
    identities: NodeIdentities,
    history: Res<TileFocusHistory>,
    selection: Res<TileSelection>,
    changed: Query<(), TileChanges>,
    mut removed: (
        RemovedComponents<TileContainer>,
        RemovedComponents<Workspace>,
        RemovedComponents<ManagedWindow>,
        RemovedComponents<TileGeometry>,
    ),
    mut published: ResMut<TileInventory>,
) {
    let removed = removed.0.read().count()
        + removed.1.read().count()
        + removed.2.read().count()
        + removed.3.read().count();
    if !published.is_added()
        && changed.is_empty()
        && removed == 0
        && !history.is_changed()
        && !selection.is_changed()
    {
        return;
    }
    let mut next = TileInventory {
        selected: selection
            .container()
            .and_then(|entity| identities.resolve(entity)),
        ..Default::default()
    };
    for (entity, container, geometry) in &containers {
        let Some(id) = identities.resolve(entity) else {
            continue;
        };
        let children: Vec<_> = container
            .children()
            .filter_map(|(entity, _)| identities.resolve(entity))
            .collect();
        let focus_history = history
            .recent()
            .filter_map(|entity| identities.resolve(entity))
            .filter(|id| children.contains(id))
            .collect();
        next.containers.insert(
            id,
            TileSnapshot {
                geometry: geometry.0,
                layout: container.layout(),
                children,
                focus_history,
            },
        );
    }
    published.set_if_neq(next);
}
