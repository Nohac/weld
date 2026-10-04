//! Shared native window registration, XDG lifecycle, commits and surface indexing.

use std::{collections::HashMap, hash::Hash, sync::Arc};

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode, shell::server::xdg_toplevel,
        },
        wayland_server::{
            Client, Resource,
            backend::ObjectId,
            protocol::{wl_buffer, wl_output::WlOutput, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::Serial,
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
            add_blocker, add_pre_commit_hook, get_role, with_states,
        },
        dmabuf::get_dmabuf,
        drm_syncobj::DrmSyncobjCachedState,
        fractional_scale::FractionalScaleHandler,
        output::OutputHandler,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData, decoration::XdgDecorationHandler,
        },
        shm::{ShmHandler, ShmState},
    },
};
use tracing::{debug, info, warn};
use weld_client::{
    ClientId, ClientSurfaceMetadata, ClientSurfaceRole, ToplevelState as ClientToplevelState,
};

use crate::{
    OutputId,
    surface::{Extent, SurfaceId, WindowDecoration, WindowResizeEdge},
};

use super::{
    ClientState, PendingSurfaceEvent, PendingSurfaceEventKind, ServerState,
    output::{send_preferred_surface_scale, send_surface_scale},
    resize::PendingResize,
    surface_tree::{
        SurfaceTreeState, collect_surfaces, owning_root, release_untracked_surface_tree,
    },
    window::WindowSurface,
};

pub(super) struct ToplevelState {
    pub(super) surface: WindowSurface,
    pub(super) decoration: WindowDecoration,
    pub(super) parent: Option<SurfaceId>,
    pub(super) hints: weld_client::ToplevelHints,
    layout: Option<weld_client::ToplevelLayout>,
    pub(super) tree: SurfaceTreeState,
    pub(super) outputs: SurfaceOutputAssignment,
    pub(super) preferred_scale_120: Option<u32>,
    resize_sources: ToplevelResizeSources,
    fullscreen: Option<bool>,
    // Consumed on first configure, not reapplied when this toplevel remaps.
    initial_size: Option<Extent>,
}

impl ToplevelState {
    pub(super) fn x11(
        window: Arc<smithay::xwayland::X11Surface>,
        surface: WlSurface,
        output: OutputId,
    ) -> Self {
        Self {
            surface: WindowSurface::X11 { window, surface },
            decoration: WindowDecoration::ServerSide,
            parent: None,
            hints: Default::default(),
            layout: Default::default(),
            tree: SurfaceTreeState::default(),
            outputs: SurfaceOutputAssignment::primary(output),
            preferred_scale_120: None,
            resize_sources: ToplevelResizeSources::default(),
            fullscreen: None,
            initial_size: None,
        }
    }
}

#[derive(Default)]
struct ToplevelResizeSources {
    protocol_grab: bool,
    policy_request: bool,
}

impl ToplevelResizeSources {
    const fn active(&self) -> bool {
        self.protocol_grab || self.policy_request
    }

    fn set(&mut self, source: ResizeSource, active: bool) -> bool {
        let was_active = self.active();
        match source {
            ResizeSource::ProtocolGrab => self.protocol_grab = active,
            ResizeSource::PolicyRequest => self.policy_request = active,
        }
        was_active != self.active()
    }
}

#[derive(Clone, Copy)]
enum ResizeSource {
    ProtocolGrab,
    PolicyRequest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SurfaceOutputAssignment {
    memberships: Vec<OutputId>,
    pub(super) preferred: OutputId,
}

impl SurfaceOutputAssignment {
    pub(super) fn primary(output: OutputId) -> Self {
        Self {
            memberships: vec![output],
            preferred: output,
        }
    }
}

pub(super) struct IndexedStore<K, V> {
    by_id: HashMap<SurfaceId, V>,
    id_by_key: HashMap<K, SurfaceId>,
}

impl<K, V> Default for IndexedStore<K, V> {
    fn default() -> Self {
        Self {
            by_id: HashMap::new(),
            id_by_key: HashMap::new(),
        }
    }
}

impl<K: Clone + Eq + Hash, V> IndexedStore<K, V> {
    pub(super) fn insert(&mut self, id: SurfaceId, key: K, value: V) -> bool {
        if self.by_id.contains_key(&id) || self.id_by_key.contains_key(&key) {
            return false;
        }
        self.id_by_key.insert(key, id);
        self.by_id.insert(id, value);
        true
    }

    pub(super) fn get(&self, id: SurfaceId) -> Option<&V> {
        self.by_id.get(&id)
    }

    pub(super) fn get_mut(&mut self, id: SurfaceId) -> Option<&mut V> {
        self.by_id.get_mut(&id)
    }

    pub(super) fn id_for_key(&self, key: &K) -> Option<SurfaceId> {
        self.id_by_key.get(key).copied()
    }

    pub(super) fn remove_by_key(&mut self, key: &K) -> Option<(SurfaceId, V)> {
        let id = self.id_by_key.remove(key)?;
        self.by_id.remove(&id).map(|value| (id, value))
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &V> {
        self.by_id.values()
    }
}

#[derive(Default)]
pub(super) struct ToplevelStore(IndexedStore<ObjectId, ToplevelState>);

impl ToplevelStore {
    pub(super) fn insert(&mut self, id: SurfaceId, state: ToplevelState) -> bool {
        let object_id = state.surface.wl_surface().id();
        self.0.insert(id, object_id, state)
    }

    pub(super) fn get(&self, id: SurfaceId) -> Option<&ToplevelState> {
        self.0.get(id)
    }

    pub(super) fn get_mut(&mut self, id: SurfaceId) -> Option<&mut ToplevelState> {
        self.0.get_mut(id)
    }

    pub(super) fn id_for_surface(&self, surface: &WlSurface) -> Option<SurfaceId> {
        self.0.id_for_key(&surface.id())
    }

    pub(super) fn remove_surface(
        &mut self,
        surface: &WlSurface,
    ) -> Option<(SurfaceId, ToplevelState)> {
        self.0.remove_by_key(&surface.id())
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &ToplevelState> {
        self.0.values()
    }
}

pub(super) fn allocate_surface_id(next: &mut Option<u64>, client: ClientId) -> Option<SurfaceId> {
    // SurfaceId values are process-unique and never wrap or reuse. Exhaustion
    // is terminal for new toplevel registration.
    let raw = (*next)?;
    *next = raw.checked_add(1);
    Some(SurfaceId::new(client, raw))
}

pub(super) fn client_id_for_surface(surface: &WlSurface) -> Option<ClientId> {
    surface
        .client()
        .and_then(|client| client.get_data::<ClientState>().map(|state| state.id))
}

impl ServerState {
    pub(super) fn close_toplevel(&self, surface: SurfaceId) {
        let Some(toplevel) = self.toplevels.get(surface) else {
            warn!(?surface, "ignored a close request for an unknown surface");
            return;
        };
        toplevel.surface.close();
    }

    pub(super) fn configure_toplevel(
        &mut self,
        surface: SurfaceId,
        requested: Extent,
        resizing: bool,
        fullscreen: bool,
        layout: weld_client::ToplevelLayout,
    ) {
        let layout_changed = self.set_toplevel_layout(surface, layout);
        let fullscreen_changed = self.set_toplevel_fullscreen(surface, fullscreen);
        let size_changed = self.stage_toplevel_size(surface, requested);
        let state_changed = self.set_toplevel_resize_source(
            surface,
            ResizeSource::PolicyRequest,
            resizing && !fullscreen,
        );
        self.send_pending_toplevel_configure(
            surface,
            size_changed || state_changed || fullscreen_changed || layout_changed,
        );
    }

    fn set_toplevel_layout(
        &mut self,
        surface: SurfaceId,
        layout: weld_client::ToplevelLayout,
    ) -> bool {
        let Some(toplevel) = self.toplevels.get_mut(surface) else {
            return false;
        };
        if toplevel.layout == Some(layout) {
            return false;
        }
        toplevel.layout = Some(layout);
        toplevel.surface.set_layout(layout);
        true
    }

    fn set_toplevel_fullscreen(&mut self, surface: SurfaceId, fullscreen: bool) -> bool {
        if fullscreen {
            self.set_toplevel_resize_source(surface, ResizeSource::ProtocolGrab, false);
        }
        let Some(toplevel) = self.toplevels.get_mut(surface) else {
            return false;
        };
        if toplevel.fullscreen == Some(fullscreen) {
            return false;
        }
        toplevel.fullscreen = Some(fullscreen);
        toplevel.surface.set_fullscreen(fullscreen);
        true
    }

    pub(super) fn begin_protocol_resize(&mut self, surface: SurfaceId) {
        let changed = self.set_toplevel_resize_source(surface, ResizeSource::ProtocolGrab, true);
        self.send_pending_toplevel_configure(surface, changed);
    }

    pub(super) fn finish_protocol_resize(
        &mut self,
        surface: SurfaceId,
        pending: Option<PendingResize>,
    ) {
        let layout_changed =
            pending.is_some_and(|request| self.set_toplevel_layout(surface, request.layout));
        let fullscreen_changed = pending
            .is_some_and(|request| self.set_toplevel_fullscreen(surface, request.fullscreen));
        let size_changed =
            pending.is_some_and(|request| self.stage_toplevel_size(surface, request.logical_size));
        let policy_changed = pending.is_some_and(|request| {
            self.set_toplevel_resize_source(
                surface,
                ResizeSource::PolicyRequest,
                request.resizing && !request.fullscreen,
            )
        });
        let protocol_changed =
            self.set_toplevel_resize_source(surface, ResizeSource::ProtocolGrab, false);
        self.send_pending_toplevel_configure(
            surface,
            size_changed
                || policy_changed
                || protocol_changed
                || fullscreen_changed
                || layout_changed,
        );
    }

    fn set_toplevel_resize_source(
        &mut self,
        surface: SurfaceId,
        source: ResizeSource,
        active: bool,
    ) -> bool {
        let Some(toplevel) = self.toplevels.get_mut(surface) else {
            return false;
        };
        let effective_state_changed = toplevel.resize_sources.set(source, active);
        let is_active = toplevel.resize_sources.active();
        if !effective_state_changed || !toplevel.surface.alive() {
            return false;
        }
        toplevel.surface.set_resizing(is_active)
    }

    fn send_pending_toplevel_configure(&self, surface: SurfaceId, changed: bool) {
        let Some(toplevel) = self.toplevels.get(surface) else {
            return;
        };
        if changed {
            toplevel.surface.flush_configure();
        }
    }

    pub(super) fn stage_toplevel_size(&self, surface: SurfaceId, requested: Extent) -> bool {
        let Some(toplevel) = self.toplevels.get(surface) else {
            warn!(?surface, "ignored a resize request for an unknown surface");
            return false;
        };
        toplevel.surface.stage_size(
            requested,
            toplevel.fullscreen.unwrap_or(false),
            toplevel.layout.unwrap_or_default(),
        )
    }

    fn record_server_side_decoration(&mut self, surface: &ToplevelSurface) {
        let Some(surface_id) = self.toplevels.id_for_surface(surface.wl_surface()) else {
            return;
        };
        let Some(toplevel) = self.toplevels.get_mut(surface_id) else {
            return;
        };
        if toplevel.decoration == WindowDecoration::ServerSide {
            return;
        }
        toplevel.decoration = WindowDecoration::ServerSide;
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: surface_id,
            kind: PendingSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ClientToplevelState {
                parent: toplevel.parent,
                decoration: WindowDecoration::ServerSide,
                hints: toplevel.hints,
            })),
        });
    }

    pub(super) fn send_all_surface_scales(&self) {
        for layer in self.layers.0.values() {
            self.send_surface_tree_scale(layer.surface.wl_surface(), &layer.outputs, None);
        }
        for toplevel in self
            .toplevels
            .values()
            .filter(|state| state.surface.alive())
        {
            self.send_surface_tree_scale(
                toplevel.surface.wl_surface(),
                &toplevel.outputs,
                toplevel.preferred_scale_120,
            );
        }
        for popup in self.popups.values().filter(|state| state.surface.alive()) {
            if let Some(assignment) = self.output_assignment_for_root(popup.surface.wl_surface()) {
                self.send_surface_tree_scale(
                    popup.surface.wl_surface(),
                    assignment,
                    self.scale_override_for_root(popup.surface.wl_surface()),
                );
            }
        }
    }

    pub(super) fn set_toplevel_outputs(
        &mut self,
        surface_id: SurfaceId,
        memberships: &[OutputId],
        preferred: Option<OutputId>,
    ) {
        let Some(assignment) = self.resolve_output_assignment(memberships, preferred) else {
            warn!(?surface_id, "ignored an empty or unknown output assignment");
            return;
        };
        let Some(toplevel) = self.toplevels.get_mut(surface_id) else {
            return;
        };
        if toplevel.outputs == assignment {
            return;
        }
        toplevel.outputs = assignment.clone();
        let root = toplevel.surface.wl_surface().clone();
        let preferred_scale_120 = toplevel.preferred_scale_120;
        self.apply_surface_tree_outputs(&root, &assignment, preferred_scale_120);

        let popup_roots = self
            .popups
            .values()
            .filter(|popup| popup.owner == Some(surface_id) && popup.surface.alive())
            .map(|popup| popup.surface.wl_surface().clone())
            .collect::<Vec<_>>();
        for popup in popup_roots {
            self.apply_surface_tree_outputs(&popup, &assignment, preferred_scale_120);
        }
    }

    pub(super) fn set_toplevel_preferred_scale(
        &mut self,
        surface_id: SurfaceId,
        preferred_scale_120: Option<u32>,
    ) {
        if preferred_scale_120 == Some(0) {
            warn!(?surface_id, "ignored a zero preferred surface scale");
            return;
        }
        let Some(toplevel) = self.toplevels.get_mut(surface_id) else {
            return;
        };
        if toplevel.preferred_scale_120 == preferred_scale_120 {
            return;
        }
        toplevel.preferred_scale_120 = preferred_scale_120;
        let root = toplevel.surface.wl_surface().clone();
        let assignment = toplevel.outputs.clone();
        self.send_surface_tree_scale(&root, &assignment, preferred_scale_120);

        let popup_roots = self
            .popups
            .values()
            .filter(|popup| popup.owner == Some(surface_id) && popup.surface.alive())
            .map(|popup| popup.surface.wl_surface().clone())
            .collect::<Vec<_>>();
        for popup in popup_roots {
            self.send_surface_tree_scale(&popup, &assignment, preferred_scale_120);
        }
    }

    pub(super) fn apply_popup_output_assignment(&self, surface_id: SurfaceId) {
        let Some(popup) = self.popups.get(surface_id) else {
            return;
        };
        let Some(assignment) = popup.owner.and_then(|owner| {
            self.toplevels
                .get(owner)
                .map(|owner| &owner.outputs)
                .or_else(|| self.layers.0.get(owner).map(|owner| &owner.outputs))
        }) else {
            return;
        };
        self.apply_surface_tree_outputs(
            popup.surface.wl_surface(),
            assignment,
            popup
                .owner
                .and_then(|owner| self.toplevels.get(owner))
                .and_then(|owner| owner.preferred_scale_120),
        );
    }

    fn resolve_output_assignment(
        &self,
        memberships: &[OutputId],
        preferred: Option<OutputId>,
    ) -> Option<SurfaceOutputAssignment> {
        let mut memberships = memberships
            .iter()
            .copied()
            .filter(|output| self.outputs.contains_key(output))
            .collect::<Vec<_>>();
        memberships.sort_unstable();
        memberships.dedup();
        let preferred = select_preferred_output(&memberships, preferred, |output| {
            self.outputs
                .get(&output)
                .map(|output| output.native.current_scale().fractional_scale())
        })?;
        Some(SurfaceOutputAssignment {
            memberships,
            preferred,
        })
    }

    pub(super) fn apply_surface_tree_outputs(
        &self,
        root: &WlSurface,
        assignment: &SurfaceOutputAssignment,
        preferred_scale_120: Option<u32>,
    ) {
        for surface in collect_surfaces(root)
            .into_iter()
            .filter(Resource::is_alive)
        {
            self.apply_surface_outputs(&surface, assignment, preferred_scale_120);
        }
    }

    fn apply_surface_outputs(
        &self,
        surface: &WlSurface,
        assignment: &SurfaceOutputAssignment,
        preferred_scale_120: Option<u32>,
    ) {
        for (output_id, output) in &self.outputs {
            if assignment.memberships.contains(output_id) {
                output.native.enter(surface);
            } else {
                output.native.leave(surface);
            }
        }
        if let Some(scale_120) = preferred_scale_120 {
            send_surface_scale(f64::from(scale_120) / 120.0, surface);
        } else if let Some(preferred) = self.outputs.get(&assignment.preferred) {
            send_preferred_surface_scale(&preferred.native, surface);
        }
    }

    fn output_assignment_for_root(&self, root: &WlSurface) -> Option<&SurfaceOutputAssignment> {
        if let Some(surface) = self.layers.id_for_surface(root) {
            return self.layers.0.get(surface).map(|state| &state.outputs);
        }
        if let Some(surface) = self.toplevels.id_for_surface(root) {
            return self.toplevels.get(surface).map(|state| &state.outputs);
        }
        let popup = self
            .popups
            .id_for_surface(root)
            .and_then(|surface| self.popups.get(surface))?;
        popup.owner.and_then(|owner| {
            self.toplevels
                .get(owner)
                .map(|state| &state.outputs)
                .or_else(|| self.layers.0.get(owner).map(|state| &state.outputs))
        })
    }

    fn scale_override_for_root(&self, root: &WlSurface) -> Option<u32> {
        if let Some(surface) = self.toplevels.id_for_surface(root) {
            return self
                .toplevels
                .get(surface)
                .and_then(|state| state.preferred_scale_120);
        }
        let popup = self
            .popups
            .id_for_surface(root)
            .and_then(|surface| self.popups.get(surface))?;
        popup
            .owner
            .and_then(|owner| self.toplevels.get(owner))
            .and_then(|state| state.preferred_scale_120)
    }

    fn send_surface_tree_scale(
        &self,
        root: &WlSurface,
        assignment: &SurfaceOutputAssignment,
        preferred_scale_120: Option<u32>,
    ) {
        if let Some(scale_120) = preferred_scale_120 {
            for surface in collect_surfaces(root)
                .into_iter()
                .filter(Resource::is_alive)
            {
                send_surface_scale(f64::from(scale_120) / 120.0, &surface);
            }
            return;
        }
        let Some(preferred) = self.outputs.get(&assignment.preferred) else {
            return;
        };
        for surface in collect_surfaces(root)
            .into_iter()
            .filter(Resource::is_alive)
        {
            send_preferred_surface_scale(&preferred.native, &surface);
        }
    }

    pub(super) fn mapped_frame_roots(&self) -> impl Iterator<Item = (SurfaceId, WlSurface)> + '_ {
        self.toplevels
            .values()
            .filter(|toplevel| {
                let root = toplevel.surface.wl_surface();
                toplevel.surface.alive() && toplevel.tree.client_mapped(root)
            })
            .filter_map(|toplevel| {
                let root = toplevel.surface.wl_surface();
                self.toplevels
                    .id_for_surface(root)
                    .map(|id| (id, root.clone()))
            })
            .chain(
                self.popups
                    .values()
                    .filter(|popup| {
                        let root = popup.surface.wl_surface();
                        popup.surface.alive() && popup.tree.client_mapped(root)
                    })
                    .filter_map(|popup| {
                        let root = popup.surface.wl_surface();
                        self.popups
                            .id_for_surface(root)
                            .map(|id| (id, root.clone()))
                    }),
            )
            .chain(self.layers.0.values().filter_map(|layer| {
                let root = layer.surface.wl_surface();
                (layer.surface.layer_surface().alive() && layer.tree.client_mapped(root))
                    .then(|| {
                        self.layers
                            .id_for_surface(root)
                            .map(|id| (id, root.clone()))
                    })
                    .flatten()
            }))
            .filter(|(_, root)| root.is_alive())
    }

    pub(crate) fn stage_frame_callbacks(&mut self) -> u64 {
        self.presentation_requested = false;
        let presentation_id = self.next_presentation_id;
        self.next_presentation_id = self.next_presentation_id.saturating_add(1);
        let mut callbacks = Vec::new();
        for (id, root) in self.mapped_frame_roots() {
            if self.presentation_claims.native(id) {
                callbacks.extend(super::presentation::take_callbacks(id, &root));
            }
        }
        self.staged_frame_callbacks
            .push_back((presentation_id, callbacks));
        presentation_id
    }

    pub(crate) fn complete_frame_callbacks(&mut self, presentation_id: u64) {
        let time = self.event_time();
        while self
            .staged_frame_callbacks
            .front()
            .is_some_and(|(staged_id, _)| *staged_id <= presentation_id)
        {
            let Some((_, callbacks)) = self.staged_frame_callbacks.pop_front() else {
                break;
            };
            if !callbacks.is_empty() {
                tracing::trace!(target: "weld_surface_diag", presentation_id, time, count = callbacks.len(), "completed frame callbacks");
            }
            for group in callbacks {
                group.complete(time);
            }
        }
    }

    pub(super) fn update_surface_tree(&mut self, surface_id: SurfaceId, root: &WlSurface) {
        let (toplevels, releases) = (&mut self.toplevels, &mut self.dmabuf_releases);
        let Some(toplevel) = toplevels.get_mut(surface_id) else {
            return;
        };
        let geometry = toplevel.surface.geometry();
        if matches!(toplevel.surface, WindowSurface::Xdg(_))
            && let hints = toplevel.surface.hints()
            && hints != toplevel.hints
        {
            toplevel.hints = hints;
            self.pending_surface_events.push_back(PendingSurfaceEvent {
                surface: surface_id,
                kind: PendingSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(
                    ClientToplevelState {
                        parent: toplevel.parent,
                        decoration: toplevel.decoration,
                        hints,
                    },
                )),
            });
        }
        let snapshot = toplevel.tree.update(surface_id, root, releases, geometry);
        if snapshot.root.is_none() {
            self.clear_input_focus_for_surface(root, self.event_time());
        }
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: surface_id,
            kind: PendingSurfaceEventKind::TreeSnapshot(snapshot),
        });
    }
}

fn select_preferred_output(
    memberships: &[OutputId],
    preferred: Option<OutputId>,
    mut scale: impl FnMut(OutputId) -> Option<f64>,
) -> Option<OutputId> {
    preferred
        .filter(|preferred| memberships.contains(preferred))
        .or_else(|| {
            memberships
                .iter()
                .copied()
                .filter_map(|output| scale(output).map(|scale| (output, scale)))
                .max_by(|left, right| left.1.total_cmp(&right.1))
                .map(|(output, _)| output)
        })
}

impl BufferHandler for ServerState {
    fn buffer_destroyed(&mut self, buffer: &wl_buffer::WlBuffer) {
        if let Ok(dmabuf) = get_dmabuf(buffer) {
            if let Some(imported) = self.dmabuf_sources.get(dmabuf) {
                self.pending_surface_events.retire_dmabuf(imported.id);
            }
            self.dmabuf_sources.remove(dmabuf);
        }
        self.dmabuf_releases.destroyed(buffer);
    }
}

impl ShmHandler for ServerState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl FractionalScaleHandler for ServerState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let root = owning_root(&surface);
        if let Some(assignment) = self.output_assignment_for_root(&root).cloned() {
            let scale = self.scale_override_for_root(&root);
            self.apply_surface_outputs(&surface, &assignment, scale);
        } else {
            send_preferred_surface_scale(self.primary_output(), &surface);
        }
    }
}

impl CompositorHandler for ServerState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        super::xwayland::compositor_state(client)
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        if self.dmabuf_blocker_installer.is_none() && self.syncobj_blocker_installer.is_none() {
            return;
        }
        add_pre_commit_hook::<Self, _>(surface, |state, _, surface| {
            let (dmabuf, acquire_point) = with_states(surface, |states| {
                let dmabuf = states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                    .and_then(|assignment| match assignment {
                        smithay::wayland::compositor::BufferAssignment::NewBuffer(buffer) => {
                            get_dmabuf(buffer).cloned().ok()
                        }
                        _ => None,
                    });
                let acquire_point = states
                    .cached_state
                    .get::<DrmSyncobjCachedState>()
                    .pending()
                    .acquire_point
                    .clone();
                (dmabuf, acquire_point)
            });
            let Some(dmabuf) = dmabuf else {
                return;
            };
            let Some(client) = surface.client() else {
                return;
            };
            if let Some(acquire_point) = acquire_point {
                match acquire_point.generate_blocker() {
                    Ok((blocker, source)) => {
                        let installed = state
                            .syncobj_blocker_installer
                            .as_ref()
                            .is_some_and(|install| install(source, client.clone()));
                        if installed {
                            add_blocker(surface, blocker);
                            return;
                        }
                        warn!(surface = ?surface.id(), "could not install an explicit-sync acquire blocker");
                    }
                    Err(error) => warn!(
                        surface = ?surface.id(),
                        %error,
                        "could not create an explicit-sync acquire blocker"
                    ),
                }
            }
            let Ok((blocker, source)) =
                dmabuf.generate_blocker(smithay::reexports::calloop::Interest::READ)
            else {
                return;
            };
            let installed = state
                .dmabuf_blocker_installer
                .as_ref()
                .is_some_and(|install| install(source, client));
            if installed {
                add_blocker(surface, blocker);
            } else {
                warn!(surface = ?surface.id(), "could not install a DMA-BUF readiness blocker");
            }
        });
    }

    fn new_subsurface(&mut self, surface: &WlSurface, parent: &WlSurface) {
        let root = owning_root(parent);
        if let Some(assignment) = self.output_assignment_for_root(&root).cloned() {
            let scale = self.scale_override_for_root(&root);
            // An attached surface can already have descendants. Move the whole
            // subtree to its new root's output assignment before its first commit.
            self.apply_surface_tree_outputs(surface, &assignment, scale);
        } else {
            self.enter_primary_output(surface);
        }
    }

    fn commit(&mut self, surface: &WlSurface) {
        if self.commit_cursor_surface(surface) {
            return;
        }
        if !SurfaceTreeState::should_process_commit(surface) {
            return;
        }
        let root = owning_root(surface);
        tracing::trace!(target: "weld_surface_diag", surface = ?surface.id(), root = ?root.id(), "processed surface commit");
        let Some(surface_id) = self.toplevels.id_for_surface(&root) else {
            if !self.commit_layer(&root) && !self.commit_popup(&root) {
                if get_role(&root).is_some_and(|role| {
                    role != smithay::wayland::xwayland_shell::XWAYLAND_SHELL_ROLE
                        || !super::xwayland::awaiting_x11_association(&root)
                }) {
                    release_untracked_surface_tree(&root);
                }
                debug!(surface = ?surface.id(), "ignoring a surface outside a tracked xdg surface tree");
            }
            return;
        };
        let Some(toplevel) = self.toplevels.get(surface_id) else {
            return;
        };
        if matches!(&toplevel.surface, WindowSurface::Xdg(window) if !window.is_initial_configure_sent())
        {
            let initial_size = self
                .toplevels
                .get_mut(surface_id)
                .and_then(|state| state.initial_size.take());
            if let Some(size) = initial_size {
                self.stage_toplevel_size(surface_id, size);
            }
            let Some(toplevel) = self.toplevels.get(surface_id) else {
                return;
            };
            if let WindowSurface::Xdg(window) = &toplevel.surface {
                window.send_configure();
            }
            return;
        }
        self.presentation_requested = true;
        self.update_surface_tree(surface_id, &root);
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        self.remove_cursor_surface(surface);
        self.leave_all_outputs(surface);
        let root = owning_root(surface);
        let Some(surface_id) = self.toplevels.id_for_surface(&root) else {
            if !self.remove_layer_subsurface(&root, surface) {
                self.remove_popup_surface(&root, surface);
            }
            return;
        };
        self.clear_input_focus_for_surface(surface, self.event_time());
        let Some(toplevel) = self.toplevels.get_mut(surface_id) else {
            return;
        };
        let snapshot = toplevel.tree.remove_surface(&root, surface);
        self.presentation_requested = true;
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: surface_id,
            kind: PendingSurfaceEventKind::TreeSnapshot(snapshot),
        });
    }
}

impl XdgShellHandler for ServerState {
    fn title_changed(&mut self, surface: ToplevelSurface) {
        self.publish_toplevel_metadata(&surface);
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.publish_toplevel_metadata(&surface);
    }
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let Some(client) = client_id_for_surface(surface.wl_surface()) else {
            warn!("refused an xdg-toplevel without a registered client identity");
            surface.send_close();
            return;
        };
        let Some(id) = allocate_surface_id(&mut self.next_surface_id, client) else {
            warn!("refused an xdg-toplevel because SurfaceId space is exhausted");
            surface.send_close();
            return;
        };
        let surface_handle = surface.clone();
        let state = ToplevelState {
            surface: WindowSurface::Xdg(surface),
            decoration: WindowDecoration::ClientSide,
            parent: None,
            hints: Default::default(),
            layout: Default::default(),
            tree: SurfaceTreeState::default(),
            outputs: SurfaceOutputAssignment::primary(self.primary_output),
            preferred_scale_120: None,
            resize_sources: ToplevelResizeSources::default(),
            fullscreen: None,
            initial_size: self.initial_toplevel_size,
        };
        if !self.toplevels.insert(id, state) {
            warn!(?id, "refused a duplicate xdg-toplevel registration");
            surface_handle.send_close();
            return;
        }
        self.apply_surface_tree_outputs(
            surface_handle.wl_surface(),
            &SurfaceOutputAssignment::primary(self.primary_output),
            None,
        );
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: id,
            kind: PendingSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ClientToplevelState {
                parent: None,
                decoration: WindowDecoration::ClientSide,
                hints: Default::default(),
            })),
        });
        info!(surface_id = id.local(), "created a nested xdg-toplevel");
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        self.register_popup(surface, positioner);
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        let Some(surface_id) = self.toplevels.id_for_surface(surface.wl_surface()) else {
            return;
        };
        let parent = surface
            .parent()
            .as_ref()
            .and_then(|parent| self.toplevels.id_for_surface(parent));
        if let Some(toplevel) = self.toplevels.get_mut(surface_id) {
            toplevel.parent = parent;
        }
        let Some(toplevel) = self.toplevels.get(surface_id) else {
            return;
        };
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: surface_id,
            kind: PendingSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ClientToplevelState {
                parent,
                decoration: toplevel.decoration,
                hints: toplevel.hints,
            })),
        });
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        self.begin_pointer_move(surface, seat, serial);
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
        self.publish_fullscreen_request(surface.wl_surface(), true);
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.publish_fullscreen_request(surface.wl_surface(), false);
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let Some(edges) = window_resize_edge(edges) else {
            return;
        };
        self.begin_pointer_resize(surface, seat, serial, edges);
    }

    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        self.begin_popup_grab(surface, seat, serial);
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        self.reposition_popup(surface, positioner, token);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.retire_window(surface.wl_surface());
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        self.destroy_popup(surface);
    }
}

impl ServerState {
    pub(super) fn retire_window(&mut self, wl_surface: &WlSurface) {
        let Some((id, _state)) = self.toplevels.remove_surface(wl_surface) else {
            return;
        };
        self.forget_presentation(id);
        self.clear_input_focus_for_surface(wl_surface, self.event_time());
        self.leave_all_outputs(wl_surface);
        if self.focused_toplevel == Some(id) {
            self.focused_toplevel = None;
        }
        self.pending_resizes.discard(id);
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: id,
            kind: PendingSurfaceEventKind::Destroyed,
        });
    }
}

impl ServerState {
    pub(super) fn publish_fullscreen_request(&mut self, surface: &WlSurface, enabled: bool) {
        if let Some(surface) = self.toplevels.id_for_surface(surface) {
            self.pending_surface_events.push_back(PendingSurfaceEvent {
                surface,
                kind: PendingSurfaceEventKind::WindowStateRequest(
                    weld_client::ToplevelStateRequestKind::Fullscreen(enabled),
                ),
            });
            self.presentation_requested = true;
        }
    }
}

impl ServerState {
    fn publish_toplevel_metadata(&mut self, surface: &ToplevelSurface) {
        let Some(id) = self.toplevels.id_for_surface(surface.wl_surface()) else {
            return;
        };
        let metadata = with_states(surface.wl_surface(), |states| {
            let data = states.data_map.get::<XdgToplevelSurfaceData>()?;
            let data = data.lock().ok()?;
            Some(ClientSurfaceMetadata::truncated(
                data.app_id.clone().unwrap_or_default(),
                data.title.clone().unwrap_or_default(),
            ))
        });
        if let Some(metadata) = metadata {
            self.pending_surface_events.push_back(PendingSurfaceEvent {
                surface: id,
                kind: PendingSurfaceEventKind::Metadata(metadata),
            });
        }
    }
}

fn stage_server_side_decoration(toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|state| {
        state.decoration_mode = Some(Mode::ServerSide);
    });
}

impl XdgDecorationHandler for ServerState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        stage_server_side_decoration(&toplevel);
        self.record_server_side_decoration(&toplevel);
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: Mode) {
        stage_server_side_decoration(&toplevel);
        self.record_server_side_decoration(&toplevel);
        if toplevel.is_initial_configure_sent() {
            // Respond even when the client requested client-side decorations and the
            // compositor's server-side mode therefore did not change.
            toplevel.send_configure();
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        stage_server_side_decoration(&toplevel);
        self.record_server_side_decoration(&toplevel);
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
    }
}

fn window_resize_edge(edges: xdg_toplevel::ResizeEdge) -> Option<WindowResizeEdge> {
    match edges {
        xdg_toplevel::ResizeEdge::Top => Some(WindowResizeEdge::Top),
        xdg_toplevel::ResizeEdge::Bottom => Some(WindowResizeEdge::Bottom),
        xdg_toplevel::ResizeEdge::Left => Some(WindowResizeEdge::Left),
        xdg_toplevel::ResizeEdge::Right => Some(WindowResizeEdge::Right),
        xdg_toplevel::ResizeEdge::TopLeft => Some(WindowResizeEdge::TopLeft),
        xdg_toplevel::ResizeEdge::BottomLeft => Some(WindowResizeEdge::BottomLeft),
        xdg_toplevel::ResizeEdge::TopRight => Some(WindowResizeEdge::TopRight),
        xdg_toplevel::ResizeEdge::BottomRight => Some(WindowResizeEdge::BottomRight),
        xdg_toplevel::ResizeEdge::None => None,
        _ => None,
    }
}

impl OutputHandler for ServerState {
    fn output_bound(&mut self, output: Output, resource: WlOutput) {
        self.workspace_output_bound(output, resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_sources_keep_the_effective_state_active_until_all_sources_end() {
        let mut sources = ToplevelResizeSources::default();

        assert!(sources.set(ResizeSource::ProtocolGrab, true));
        assert!(sources.active());
        assert!(!sources.set(ResizeSource::PolicyRequest, true));
        assert!(sources.active());
        assert!(!sources.set(ResizeSource::ProtocolGrab, false));
        assert!(sources.active());
        assert!(sources.set(ResizeSource::PolicyRequest, false));
        assert!(!sources.active());
    }

    #[test]
    fn indexed_store_keeps_multiple_values_and_removes_only_the_target() {
        let mut store = IndexedStore::<u32, &'static str>::default();
        let first = SurfaceId::for_test(1);
        let second = SurfaceId::for_test(2);
        assert!(store.insert(first, 10, "first"));
        assert!(store.insert(second, 20, "second"));
        assert_eq!(store.id_for_key(&10), Some(first));
        assert_eq!(store.id_for_key(&20), Some(second));

        assert_eq!(store.remove_by_key(&10), Some((first, "first")));
        assert_eq!(store.get(second), Some(&"second"));
        assert_eq!(store.id_for_key(&20), Some(second));
    }

    #[test]
    fn indexed_store_rejects_duplicate_ids_and_keys() {
        let mut store = IndexedStore::<u32, &'static str>::default();
        let first = SurfaceId::for_test(1);
        assert!(store.insert(first, 10, "first"));
        assert!(!store.insert(first, 20, "duplicate id"));
        assert!(!store.insert(SurfaceId::for_test(2), 10, "duplicate key"));
    }

    #[test]
    fn surface_ids_exhaust_without_wrapping() {
        let mut next = Some(u64::MAX);
        let client = ClientId::new(weld_client::ClientSourceId::new(0), 0);
        assert_eq!(
            allocate_surface_id(&mut next, client),
            Some(SurfaceId::for_test(u64::MAX))
        );
        assert_eq!(next, None);
        assert_eq!(allocate_surface_id(&mut next, client), None);
    }

    #[test]
    fn allocated_surface_identity_retains_its_wayland_client() {
        let client = ClientId::new(weld_client::ClientSourceId::new(0), 42);
        let mut next = Some(7);

        let surface = allocate_surface_id(&mut next, client).expect("available identity");

        assert_eq!(surface.client(), client);
        assert_eq!(surface.local(), 7);
    }

    #[test]
    fn preferred_output_policy_preserves_explicit_membership_and_uses_highest_scale_fallback() {
        let first = OutputId::new(1);
        let second = OutputId::new(2);
        let memberships = [first, second];

        assert_eq!(
            select_preferred_output(&memberships, Some(first), |output| {
                Some(if output == first { 1.0 } else { 1.5 })
            }),
            Some(first)
        );
        assert_eq!(
            select_preferred_output(&memberships, None, |output| {
                Some(if output == first { 1.0 } else { 1.5 })
            }),
            Some(second)
        );
        assert_eq!(
            select_preferred_output(&memberships, Some(OutputId::new(3)), |output| {
                Some(if output == first { 1.0 } else { 1.5 })
            }),
            Some(second)
        );
    }
}
