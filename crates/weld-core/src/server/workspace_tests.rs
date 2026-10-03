//! Wire-level workspace inventory, transaction and lifetime regression.

use super::*;
use crate::workspace::{DesktopWorkspace, DesktopWorkspaceId};
use wayland_client::{WEnum, event_created_child};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

#[derive(Default)]
pub(super) struct Probe {
    pub(super) manager: Option<ExtWorkspaceManagerV1>,
    pub(super) output_registry: Option<(wl_registry::WlRegistry, u32)>,
    handles: Vec<ExtWorkspaceHandleV1>,
    groups: Vec<ExtWorkspaceGroupHandleV1>,
    names: HashMap<u32, String>,
    states: HashMap<u32, ext_workspace_handle_v1::State>,
    events: Vec<String>,
    done: usize,
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for Observer {
    fn event(
        state: &mut Self,
        _: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                state.workspaces.handles.push(workspace)
            }
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                state.workspaces.groups.push(workspace_group)
            }
            ext_workspace_manager_v1::Event::Done => state.workspaces.done += 1,
            ext_workspace_manager_v1::Event::Finished => {
                state.workspaces.events.push("finished".into())
            }
            _ => {}
        }
    }
    event_created_child!(Observer, ExtWorkspaceManagerV1, [
        0 => (ExtWorkspaceGroupHandleV1, ()),
        1 => (ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for Observer {
    fn event(
        state: &mut Self,
        handle: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = handle.id().protocol_id();
        match event {
            ext_workspace_handle_v1::Event::Name { name } => {
                state.workspaces.names.insert(id, name);
            }
            ext_workspace_handle_v1::Event::State {
                state: WEnum::Value(value),
            } => {
                state.workspaces.states.insert(id, value);
            }
            ext_workspace_handle_v1::Event::Capabilities { capabilities } => assert_eq!(
                capabilities,
                WEnum::Value(ext_workspace_handle_v1::WorkspaceCapabilities::Activate)
            ),
            ext_workspace_handle_v1::Event::Removed => {
                state.workspaces.events.push(format!("removed {id}"))
            }
            ext_workspace_handle_v1::Event::Id { .. } => {
                panic!("session ID must not be advertised as persistent")
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for Observer {
    fn event(
        state: &mut Self,
        group: &ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = group.id().protocol_id();
        let event = match event {
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                format!("enter {id} {}", workspace.id().protocol_id())
            }
            ext_workspace_group_handle_v1::Event::WorkspaceLeave { workspace } => {
                format!("leave {id} {}", workspace.id().protocol_id())
            }
            ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
                format!("output {id} {}", output.id().protocol_id())
            }
            ext_workspace_group_handle_v1::Event::Removed => format!("group removed {id}"),
            _ => return,
        };
        state.workspaces.events.push(event);
    }
}

fn workspace(id: u64, output: u64, active: bool) -> DesktopWorkspace {
    DesktopWorkspace {
        id: DesktopWorkspaceId::new(id),
        name: id.to_string(),
        output: Some(OutputId::new(output)),
        active,
    }
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn empty_initial_inventory_still_completes() {
    let mut f = Fixture::new();
    f.server.outputs.clear();
    f.server.publish_workspaces(Vec::new());
    f.sync();
    f.sync();
    assert!(f.observer.workspaces.manager.is_some());
    assert!(f.observer.workspaces.groups.is_empty());
    assert!(f.observer.workspaces.handles.is_empty());
    assert_eq!(f.observer.workspaces.done, 1);
    f.server.publish_workspaces(Vec::new());
    f.sync();
    assert_eq!(f.observer.workspaces.done, 1);
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn workspace_inventory_diffs_and_commit_batched_activation() {
    let mut f = Fixture::new();
    assert!(f.observer.workspaces.manager.is_none());
    let mut snapshot = vec![workspace(1, 1, true), workspace(2, 1, false)];
    f.server.publish_workspaces(snapshot.clone());
    f.sync();
    f.sync();
    let manager = f
        .observer
        .workspaces
        .manager
        .clone()
        .expect("manager advertised");
    assert_eq!(f.observer.workspaces.handles.len(), 2);
    assert_eq!(f.observer.workspaces.groups.len(), 2);
    assert_eq!(f.observer.workspaces.done, 1);
    let first = f.observer.workspaces.handles[0].clone();
    let second = f.observer.workspaces.handles[1].clone();
    let first_id = first.id().protocol_id();
    let second_id = second.id().protocol_id();
    assert_eq!(f.observer.workspaces.names[&first_id], "1");
    assert_eq!(
        f.observer.workspaces.states[&second_id],
        ext_workspace_handle_v1::State::empty()
    );
    second.activate();
    f.sync();
    assert_eq!(
        f.server.take_workspace_activations().count(),
        0,
        "request must wait for commit"
    );
    first.activate();
    manager.commit();
    f.sync();
    assert_eq!(
        f.server.take_workspace_activations().collect::<Vec<_>>(),
        [vec![DesktopWorkspaceId::new(2), DesktopWorkspaceId::new(1)]]
    );
    f.server.publish_workspaces(snapshot.clone());
    f.sync();
    assert_eq!(
        f.observer.workspaces.done, 1,
        "unchanged snapshots are silent"
    );

    f.observer.workspaces.events.clear();
    snapshot[1].output = Some(OutputId::new(2));
    snapshot[1].name = "2: renamed".into();
    snapshot[1].active = true;
    f.server.publish_workspaces(snapshot.clone());
    f.sync();
    let groups = &f.observer.workspaces.groups;
    assert_eq!(
        f.observer.workspaces.events,
        [
            format!("leave {} {second_id}", groups[0].id().protocol_id()),
            format!("enter {} {second_id}", groups[1].id().protocol_id())
        ]
    );
    assert_eq!(f.observer.workspaces.names[&second_id], "2: renamed");
    assert_eq!(
        f.observer.workspaces.states[&second_id],
        ext_workspace_handle_v1::State::Active
    );

    second.activate();
    f.sync();
    f.observer.workspaces.events.clear();
    snapshot.pop();
    f.server.publish_workspaces(snapshot.clone());
    manager.commit();
    f.sync();
    assert_eq!(
        f.server.take_workspace_activations().count(),
        0,
        "removed requests ignored"
    );
    assert_eq!(
        f.observer.workspaces.events,
        [
            format!(
                "leave {} {second_id}",
                f.observer.workspaces.groups[1].id().protocol_id()
            ),
            format!("removed {second_id}")
        ]
    );
    second.destroy();
    first.destroy();
    f.sync();
    snapshot[0].name = "changed after client released handle".into();
    f.server.publish_workspaces(snapshot);
    f.sync();
    assert_eq!(
        f.observer.workspaces.handles.len(),
        2,
        "released handles stay released"
    );
    manager.stop();
    f.sync();
    assert_eq!(
        f.observer.workspaces.events.last().map(String::as_str),
        Some("finished")
    );
}

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn late_output_binding_and_group_removal_keep_membership_order() {
    let mut f = Fixture::new();
    let snapshot = vec![workspace(1, 2, true)];
    f.server.publish_workspaces(snapshot.clone());
    f.sync();
    f.sync();
    f.observer.workspaces.events.clear();
    let (registry, name) = f
        .observer
        .workspaces
        .output_registry
        .as_ref()
        .expect("output registry");
    let output: wl_output::WlOutput = registry.bind(*name, 4, &f.queue.handle(), ());
    f.sync();
    assert!(
        f.observer
            .workspaces
            .events
            .iter()
            .any(|event| event.ends_with(&format!(" {}", output.id().protocol_id())))
    );
    assert_eq!(
        f.observer.workspaces.done, 2,
        "late output binding is a complete update"
    );
    f.observer.workspaces.events.clear();
    let group = f.observer.workspaces.groups[1].id().protocol_id();
    let handle = f.observer.workspaces.handles[0].id().protocol_id();
    f.server.outputs.remove(&OutputId::new(2));
    f.server.publish_workspaces(snapshot);
    f.sync();
    assert_eq!(
        f.observer.workspaces.events,
        [
            format!("leave {group} {handle}"),
            format!("group removed {group}")
        ]
    );
}
