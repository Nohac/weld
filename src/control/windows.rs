//! Publishes the window owner's inventory to native control consumers.

use crate::agent::ControlledWindow;
use bevy::prelude::*;
use weld_app::inspection::{InspectionPlugin, InspectionSystems};
use weld_control::windows::{Rect, WindowInfo, WindowService};
use weld_window::ManagedWindow;
use weld_window::inspection::WindowInventory;

pub(crate) struct NativeWindowPlugin(pub WindowService);

#[derive(Resource)]
struct Publisher(WindowService);

impl Plugin for NativeWindowPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<InspectionPlugin>() {
            app.add_plugins(InspectionPlugin);
        }
        app.insert_resource(Publisher(self.0.clone()))
            .add_systems(Last, publish.in_set(InspectionSystems::Publish));
    }
}

fn publish(
    windows: Res<WindowInventory>,
    publisher: Res<Publisher>,
    controlled: Query<(&ManagedWindow, Ref<ControlledWindow>)>,
    mut retired: RemovedComponents<ControlledWindow>,
) {
    let removed = retired.read().count() > 0;
    if !windows.is_changed()
        && !publisher.is_added()
        && !removed
        && !controlled.iter().any(|(_, control)| control.is_changed())
    {
        return;
    }
    let snapshot = windows
        .windows()
        .iter()
        .map(|(id, window)| WindowInfo {
            id: id.raw().to_string(),
            app_id: window.occupant.as_ref().map(|app| app.app_id().to_owned()),
            title: window.occupant.as_ref().map(|app| app.title().to_owned()),
            workspace: window
                .workspace
                .map(|workspace| workspace.raw().to_string()),
            output: window.output.map(|output| output.raw().to_string()),
            geometry: Rect {
                x: window.geometry.position.x,
                y: window.geometry.position.y,
                width: window.geometry.size.x,
                height: window.geometry.size.y,
            },
            occupied: window.occupant.is_some(),
            focused: windows.focused() == Some(*id),
            visible: window.visible,
            floating: window.floating,
            control: controlled
                .iter()
                .find(|(window, control)| window.id == *id && control.0.active())
                .map(|(_, control)| control.0.id().to_owned()),
        })
        .collect();
    if let Err(error) = publisher.0.publish(snapshot) {
        tracing::warn!(%error, "could not publish native window inventory");
    }
}
