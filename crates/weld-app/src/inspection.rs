//! Publication stages and output inventory for desktop inspection consumers.

use crate::output::{
    OutputGeometry, OutputId, OutputInfo, OutputPosition, PrimaryOutput, WeldOutput,
};
use bevy::{
    app::{App, Last, Plugin},
    ecs::{query::QueryData, system::SystemParam},
    prelude::*,
};
use std::collections::BTreeMap;

/// Collect each domain's current state before consumers publish a combined snapshot.
#[derive(SystemSet, Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum InspectionSystems {
    Collect,
    Publish,
}

/// Enables domain publishers registered by the installed desktop plugins.
#[derive(Resource)]
pub struct InspectionEnabled;

/// Activates change-driven desktop inspection and installs output publication.
pub struct InspectionPlugin;

impl Plugin for InspectionPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(InspectionEnabled)
            .init_resource::<OutputInventory>()
            .configure_sets(
                Last,
                (InspectionSystems::Collect, InspectionSystems::Publish).chain(),
            )
            .add_systems(Last, collect_outputs.in_set(InspectionSystems::Collect));
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutputSnapshot {
    pub name: Option<String>,
    pub position: Vec2,
    pub logical_size: Vec2,
    pub scale: f32,
    pub primary: bool,
}

/// Stable-ID output records, replaced only when their public values change.
#[derive(Resource, Default, PartialEq)]
pub struct OutputInventory(BTreeMap<OutputId, OutputSnapshot>);

impl OutputInventory {
    pub fn outputs(&self) -> &BTreeMap<OutputId, OutputSnapshot> {
        &self.0
    }
}

type OutputChanges = Or<(
    Changed<WeldOutput>,
    Changed<OutputGeometry>,
    Changed<OutputPosition>,
    Changed<OutputInfo>,
    Added<PrimaryOutput>,
)>;

#[derive(QueryData)]
struct InspectOutput {
    output: &'static WeldOutput,
    geometry: &'static OutputGeometry,
    position: &'static OutputPosition,
    info: Option<&'static OutputInfo>,
    primary: Has<PrimaryOutput>,
}

#[derive(SystemParam)]
struct RemovedOutputState<'w, 's> {
    identity: RemovedComponents<'w, 's, WeldOutput>,
    primary: RemovedComponents<'w, 's, PrimaryOutput>,
    info: RemovedComponents<'w, 's, OutputInfo>,
    geometry: RemovedComponents<'w, 's, OutputGeometry>,
    position: RemovedComponents<'w, 's, OutputPosition>,
}

fn collect_outputs(
    outputs: Query<InspectOutput>,
    changed: Query<(), OutputChanges>,
    mut removed: RemovedOutputState,
    mut published: ResMut<OutputInventory>,
) {
    let removed = removed.identity.read().count()
        + removed.primary.read().count()
        + removed.info.read().count()
        + removed.geometry.read().count()
        + removed.position.read().count();
    if !published.is_added() && changed.is_empty() && removed == 0 {
        return;
    }
    let next = outputs
        .iter()
        .map(|item| {
            (
                item.output.id,
                OutputSnapshot {
                    name: item.info.map(|info| info.name().to_owned()),
                    position: item.position.0,
                    logical_size: item.geometry.logical_size(),
                    scale: item.geometry.scale_factor(),
                    primary: item.primary,
                },
            )
        })
        .collect();
    published.set_if_neq(OutputInventory(next));
}
