//! Publishes the window owner's inventory to native control consumers.

use crate::remote_control::{ControlPolicy, ControlledWindow};
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
    policies: Query<(&ManagedWindow, Ref<ControlPolicy>)>,
    mut retired: RemovedComponents<ControlledWindow>,
    mut removed_policies: RemovedComponents<ControlPolicy>,
) {
    let removed = retired.read().count() > 0;
    let policy_removed = removed_policies.read().count() > 0;
    if !windows.is_changed()
        && !publisher.is_added()
        && !removed
        && !policy_removed
        && !policies.iter().any(|(_, policy)| policy.is_changed())
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
            wayland_process_id: window
                .occupant
                .as_ref()
                .filter(|app| app.x11_class().is_none())
                .and_then(|app| app.local_process_id()),
            desktop_slot: Rect {
                x: window.geometry.position.x,
                y: window.geometry.position.y,
                width: window.geometry.size.x,
                height: window.geometry.size.y,
            },
            application_viewport: None,
            occupied: window.occupant.is_some(),
            focused: windows.focused() == Some(*id),
            visible: window.visible,
            floating: window.floating,
            control: controlled
                .iter()
                .find(|(window, control)| window.id == *id && control.0.active())
                .map(|(_, control)| control.0.id().to_owned()),
            control_key: policies
                .iter()
                .find(|(window, _)| window.id == *id)
                .map(|(_, policy)| policy.key.clone()),
            capabilities: policies
                .iter()
                .find(|(window, _)| window.id == *id)
                .map(|(_, policy)| policy.capabilities),
        })
        .collect();
    if let Err(error) = publisher.0.publish(snapshot) {
        tracing::warn!(%error, "could not publish native window inventory");
    }
}
