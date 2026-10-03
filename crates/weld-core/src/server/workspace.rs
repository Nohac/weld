//! Local workspace discovery and commit-batched activation through ext-workspace-v1.

use super::ServerState;
use crate::{
    OutputId,
    workspace::{DesktopWorkspace, DesktopWorkspaceId},
};
use smithay::{
    output::Output,
    reexports::{
        wayland_protocols::ext::workspace::v1::server::{
            ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
            ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
            ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
        },
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource,
            backend::{ClientId, GlobalId},
            protocol::wl_output::WlOutput,
        },
    },
    wayland::{Dispatch2, GlobalDispatch2},
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const MAX_PENDING_ACTIVATIONS: usize = 256;
const MAX_COMMITTED_TRANSACTIONS: usize = 256;

#[derive(Default)]
pub(super) struct WorkspaceProtocol {
    global: Option<GlobalId>,
    snapshot: BTreeMap<DesktopWorkspaceId, DesktopWorkspace>,
    bindings: Vec<Binding>,
    committed: VecDeque<Vec<DesktopWorkspaceId>>,
}

struct Binding {
    manager: ExtWorkspaceManagerV1,
    groups: BTreeMap<OutputId, ExtWorkspaceGroupHandleV1>,
    workspaces: BTreeMap<DesktopWorkspaceId, (ExtWorkspaceHandleV1, DesktopWorkspace)>,
    ignored_workspaces: BTreeSet<DesktopWorkspaceId>,
    ignored_groups: BTreeSet<OutputId>,
    pending: Vec<DesktopWorkspaceId>,
}

pub(super) struct ManagerData;
struct WorkspaceData {
    manager: ExtWorkspaceManagerV1,
    id: DesktopWorkspaceId,
}
struct GroupData {
    manager: ExtWorkspaceManagerV1,
    output: OutputId,
}

impl ServerState {
    pub(crate) fn publish_workspaces(&mut self, workspaces: Vec<DesktopWorkspace>) {
        self.workspaces.snapshot = workspaces
            .into_iter()
            .map(|mut workspace| {
                workspace.output = workspace.output.filter(|id| self.outputs.contains_key(id));
                (workspace.id, workspace)
            })
            .collect();
        if self.workspaces.global.is_none() {
            self.workspaces.global = Some(
                self.display_handle
                    .create_global::<Self, ExtWorkspaceManagerV1, _>(1, ManagerData),
            );
        }
        let outputs = self
            .outputs
            .iter()
            .map(|(id, output)| (*id, output.native.clone()))
            .collect();
        self.workspaces
            .bindings
            .retain(|binding| binding.manager.is_alive());
        for binding in &mut self.workspaces.bindings {
            if binding.publish(&self.display_handle, &self.workspaces.snapshot, &outputs) {
                binding.manager.done();
            }
        }
    }

    pub(crate) fn take_workspace_activations(
        &mut self,
    ) -> impl Iterator<Item = Vec<DesktopWorkspaceId>> + '_ {
        self.workspaces.committed.drain(..)
    }

    pub(super) fn workspace_output_bound(&mut self, output: Output, resource: WlOutput) {
        let Some(id) = self
            .outputs
            .iter()
            .find_map(|(id, native)| (native.native == output).then_some(*id))
        else {
            return;
        };
        for binding in &self.workspaces.bindings {
            if binding.manager.client() == resource.client()
                && let Some(group) = binding.groups.get(&id)
            {
                group.output_enter(&resource);
                binding.manager.done();
            }
        }
    }
}

impl Binding {
    fn publish(
        &mut self,
        display: &DisplayHandle,
        snapshot: &BTreeMap<DesktopWorkspaceId, DesktopWorkspace>,
        outputs: &BTreeMap<OutputId, Output>,
    ) -> bool {
        let Some(client) = self.manager.client() else {
            return false;
        };
        let mut changed = false;
        self.ignored_workspaces
            .retain(|id| snapshot.contains_key(id));
        self.ignored_groups.retain(|id| outputs.contains_key(id));
        self.workspaces.retain(|id, (handle, old)| {
            if snapshot.contains_key(id) {
                return true;
            }
            if let Some(group) = old.output.and_then(|output| self.groups.get(&output)) {
                group.workspace_leave(handle);
            }
            handle.removed();
            changed = true;
            false
        });
        // Membership changes precede group removal and every workspace removal.
        for (id, (handle, old)) in &mut self.workspaces {
            let next = &snapshot[id];
            if old.output != next.output {
                if let Some(group) = old.output.and_then(|output| self.groups.get(&output)) {
                    group.workspace_leave(handle);
                }
                old.output = None;
                changed = true;
            }
        }
        self.groups.retain(|id, group| {
            if outputs.contains_key(id) {
                return true;
            }
            group.removed();
            changed = true;
            false
        });
        for (id, output) in outputs {
            if self.groups.contains_key(id) || self.ignored_groups.contains(id) {
                continue;
            }
            let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, _, ServerState>(
                display,
                1,
                GroupData {
                    manager: self.manager.clone(),
                    output: *id,
                },
            ) else {
                continue;
            };
            self.manager.workspace_group(&group);
            group.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());
            for output in output.client_outputs(&client) {
                group.output_enter(&output);
            }
            self.groups.insert(*id, group);
            changed = true;
        }
        for (id, next) in snapshot {
            if self.ignored_workspaces.contains(id) {
                continue;
            }
            let (handle, previous) = if let Some((handle, old)) = self.workspaces.get(id) {
                (handle.clone(), Some(old.clone()))
            } else {
                let Ok(handle) = client.create_resource::<ExtWorkspaceHandleV1, _, ServerState>(
                    display,
                    1,
                    WorkspaceData {
                        manager: self.manager.clone(),
                        id: *id,
                    },
                ) else {
                    continue;
                };
                self.manager.workspace(&handle);
                // Session IDs are deliberately not advertised as persistent protocol IDs.
                handle.capabilities(ext_workspace_handle_v1::WorkspaceCapabilities::Activate);
                (handle, None)
            };
            if previous.as_ref().is_none_or(|old| old.name != next.name) {
                handle.name(next.name.clone());
                changed = true;
            }
            if previous
                .as_ref()
                .is_none_or(|old| old.active != next.active)
            {
                handle.state(if next.active {
                    ext_workspace_handle_v1::State::Active
                } else {
                    ext_workspace_handle_v1::State::empty()
                });
                changed = true;
            }
            if previous
                .as_ref()
                .is_none_or(|old| old.output != next.output)
                && let Some(group) = next.output.and_then(|id| self.groups.get(&id))
            {
                group.workspace_enter(&handle);
                changed = true;
            }
            self.workspaces.insert(*id, (handle, next.clone()));
        }
        changed
    }
}

impl GlobalDispatch2<ExtWorkspaceManagerV1, ServerState> for ManagerData {
    fn bind(
        &self,
        state: &mut ServerState,
        display: &DisplayHandle,
        _: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        init: &mut DataInit<'_, ServerState>,
    ) {
        let manager = init.init(resource, ManagerData);
        let mut binding = Binding {
            manager,
            groups: BTreeMap::new(),
            workspaces: BTreeMap::new(),
            ignored_workspaces: BTreeSet::new(),
            ignored_groups: BTreeSet::new(),
            pending: Vec::new(),
        };
        let outputs = state
            .outputs
            .iter()
            .map(|(id, output)| (*id, output.native.clone()))
            .collect();
        binding.publish(display, &state.workspaces.snapshot, &outputs);
        // Complete the initial inventory even when it has no groups or workspaces.
        binding.manager.done();
        state.workspaces.bindings.push(binding);
    }
}

impl Dispatch2<ExtWorkspaceManagerV1, ServerState> for ManagerData {
    fn request(
        &self,
        state: &mut ServerState,
        _: &Client,
        resource: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _: &DisplayHandle,
        _: &mut DataInit<'_, ServerState>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                if let Some(binding) = state
                    .workspaces
                    .bindings
                    .iter_mut()
                    .find(|binding| binding.manager == *resource)
                {
                    let mut pending = std::mem::take(&mut binding.pending);
                    pending.retain(|id| {
                        state.workspaces.snapshot.contains_key(id)
                            && binding.workspaces.contains_key(id)
                    });
                    if !pending.is_empty()
                        && state.workspaces.committed.len() < MAX_COMMITTED_TRANSACTIONS
                    {
                        state.workspaces.committed.push_back(pending);
                        state.presentation_requested = true;
                    }
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                state
                    .workspaces
                    .bindings
                    .retain(|binding| binding.manager != *resource);
                resource.finished();
            }
            _ => {}
        }
    }
    fn destroyed(&self, state: &mut ServerState, _: ClientId, resource: &ExtWorkspaceManagerV1) {
        state
            .workspaces
            .bindings
            .retain(|binding| binding.manager != *resource);
    }
}

impl Dispatch2<ExtWorkspaceHandleV1, ServerState> for WorkspaceData {
    fn request(
        &self,
        state: &mut ServerState,
        _: &Client,
        resource: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        _: &DisplayHandle,
        _: &mut DataInit<'_, ServerState>,
    ) {
        if !matches!(request, ext_workspace_handle_v1::Request::Activate) {
            return;
        }
        if !state.workspaces.snapshot.contains_key(&self.id) {
            return;
        }
        if let Some(binding) = state
            .workspaces
            .bindings
            .iter_mut()
            .find(|binding| binding.manager == self.manager)
            && binding
                .workspaces
                .get(&self.id)
                .is_some_and(|(handle, _)| handle == resource)
            && binding.pending.len() < MAX_PENDING_ACTIVATIONS
        {
            binding.pending.push(self.id);
        }
    }
    fn destroyed(&self, state: &mut ServerState, _: ClientId, _: &ExtWorkspaceHandleV1) {
        if let Some(binding) = state
            .workspaces
            .bindings
            .iter_mut()
            .find(|binding| binding.manager == self.manager)
        {
            binding.workspaces.remove(&self.id);
            binding.ignored_workspaces.insert(self.id);
        }
    }
}

impl Dispatch2<ExtWorkspaceGroupHandleV1, ServerState> for GroupData {
    fn request(
        &self,
        _: &mut ServerState,
        _: &Client,
        _: &ExtWorkspaceGroupHandleV1,
        _: ext_workspace_group_handle_v1::Request,
        _: &DisplayHandle,
        _: &mut DataInit<'_, ServerState>,
    ) {
    }
    fn destroyed(&self, state: &mut ServerState, _: ClientId, _: &ExtWorkspaceGroupHandleV1) {
        if let Some(binding) = state
            .workspaces
            .bindings
            .iter_mut()
            .find(|binding| binding.manager == self.manager)
        {
            binding.groups.remove(&self.output);
            binding.ignored_groups.insert(self.output);
        }
    }
}
