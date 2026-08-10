//! XDG toplevel registration, lifecycle, commits, and surface indexing.

use std::{collections::HashMap, hash::Hash};

use smithay::{
    reexports::{
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
        wayland_server::{
            Client, Resource,
            backend::ObjectId,
            protocol::{wl_buffer, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::{Logical, Serial, Size},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
            with_states,
        },
        fractional_scale::FractionalScaleHandler,
        output::OutputHandler,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            decoration::XdgDecorationHandler,
        },
        shm::{ShmHandler, ShmState},
    },
};
use tracing::{debug, info, warn};

use crate::surface::{HostSurfaceEvent, SurfaceId};

use super::{
    ClientState, ServerState,
    output::send_preferred_surface_scale,
    surface_tree::{SurfaceTreeState, collect_surfaces, owning_root, should_drain_callbacks},
};

const CLIENT_WIDTH: i32 = 640;
const CLIENT_HEIGHT: i32 = 480;

pub(super) struct ToplevelState {
    pub(super) surface: ToplevelSurface,
    tree: SurfaceTreeState,
}

struct IndexedStore<K, V> {
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
    fn insert(&mut self, id: SurfaceId, key: K, value: V) -> bool {
        if self.by_id.contains_key(&id) || self.id_by_key.contains_key(&key) {
            return false;
        }
        self.id_by_key.insert(key, id);
        self.by_id.insert(id, value);
        true
    }

    fn get(&self, id: SurfaceId) -> Option<&V> {
        self.by_id.get(&id)
    }

    fn get_mut(&mut self, id: SurfaceId) -> Option<&mut V> {
        self.by_id.get_mut(&id)
    }

    fn id_for_key(&self, key: &K) -> Option<SurfaceId> {
        self.id_by_key.get(key).copied()
    }

    fn remove_by_key(&mut self, key: &K) -> Option<(SurfaceId, V)> {
        let id = self.id_by_key.remove(key)?;
        self.by_id.remove(&id).map(|value| (id, value))
    }

    fn values(&self) -> impl Iterator<Item = &V> {
        self.by_id.values()
    }
}

#[derive(Default)]
pub(super) struct ToplevelStore(IndexedStore<ObjectId, ToplevelState>);

impl ToplevelStore {
    fn insert(&mut self, id: SurfaceId, state: ToplevelState) -> bool {
        let object_id = state.surface.wl_surface().id();
        self.0.insert(id, object_id, state)
    }

    pub(super) fn get(&self, id: SurfaceId) -> Option<&ToplevelState> {
        self.0.get(id)
    }

    fn get_mut(&mut self, id: SurfaceId) -> Option<&mut ToplevelState> {
        self.0.get_mut(id)
    }

    pub(super) fn id_for_surface(&self, surface: &WlSurface) -> Option<SurfaceId> {
        self.0.id_for_key(&surface.id())
    }

    fn remove_surface(&mut self, surface: &WlSurface) -> Option<(SurfaceId, ToplevelState)> {
        self.0.remove_by_key(&surface.id())
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &ToplevelState> {
        self.0.values()
    }
}

pub(super) fn allocate_surface_id(next: &mut Option<u64>) -> Option<SurfaceId> {
    // SurfaceId values are process-unique and never wrap or reuse. Exhaustion
    // is terminal for new toplevel registration.
    let raw = (*next)?;
    *next = raw.checked_add(1);
    Some(SurfaceId::new(raw))
}

impl ServerState {
    pub(super) fn close_toplevel(&self, surface: SurfaceId) {
        let Some(toplevel) = self.toplevels.get(surface) else {
            warn!(?surface, "ignored a close request for an unknown surface");
            return;
        };
        toplevel.surface.send_close();
    }

    pub(super) fn send_all_surface_scales(&self) {
        let surfaces = self
            .toplevels
            .values()
            .filter(|toplevel| toplevel.surface.alive())
            .flat_map(|toplevel| collect_surfaces(toplevel.surface.wl_surface()))
            .filter(Resource::is_alive)
            .collect::<Vec<_>>();
        for surface in surfaces {
            send_preferred_surface_scale(&self.output, &surface);
        }
    }

    pub(super) fn complete_surface_presentation(&mut self) {
        let time = self.event_time();
        let surfaces = self
            .toplevels
            .values()
            .filter(|toplevel| {
                let root = toplevel.surface.wl_surface();
                toplevel.surface.alive()
                    && should_drain_callbacks(
                        toplevel.tree.client_mapped(root),
                        toplevel.tree.displayable(root),
                    )
            })
            .flat_map(|toplevel| collect_surfaces(toplevel.surface.wl_surface()))
            .filter(Resource::is_alive)
            .collect::<Vec<_>>();
        for surface in surfaces {
            with_states(&surface, |states| {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                for callback in attributes.current().frame_callbacks.drain(..) {
                    callback.done(time);
                }
            });
        }
    }

    fn update_surface_tree(&mut self, surface_id: SurfaceId, root: &WlSurface) {
        let Some(toplevel) = self.toplevels.get_mut(surface_id) else {
            return;
        };
        let snapshot = toplevel.tree.update(surface_id, root);
        if snapshot.root.is_none() {
            self.clear_input_focus_for_surface(root, self.event_time());
        }
        self.pending_surface_events
            .push(HostSurfaceEvent::TreeSnapshot {
                surface: surface_id,
                snapshot,
            });
    }
}

impl BufferHandler for ServerState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for ServerState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl FractionalScaleHandler for ServerState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        send_preferred_surface_scale(&self.output, &surface);
    }
}

impl CompositorHandler for ServerState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("Weld inserts ClientState for every accepted client")
            .compositor_state
    }

    fn new_subsurface(&mut self, surface: &WlSurface, _parent: &WlSurface) {
        self.output.enter(surface);
        send_preferred_surface_scale(&self.output, surface);
    }

    fn commit(&mut self, surface: &WlSurface) {
        if !SurfaceTreeState::should_process_commit(surface) {
            return;
        }
        let root = owning_root(surface);
        let Some(surface_id) = self.toplevels.id_for_surface(&root) else {
            debug!(surface = ?surface.id(), "ignoring a surface outside an xdg-toplevel tree");
            return;
        };
        let Some(toplevel) = self.toplevels.get(surface_id) else {
            return;
        };
        if !toplevel.surface.is_initial_configure_sent() {
            toplevel.surface.send_configure();
            return;
        }
        self.presentation_requested = true;
        self.update_surface_tree(surface_id, &root);
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        self.output.leave(surface);
        let root = owning_root(surface);
        let Some(surface_id) = self.toplevels.id_for_surface(&root) else {
            return;
        };
        let Some(toplevel) = self.toplevels.get_mut(surface_id) else {
            return;
        };
        let snapshot = toplevel.tree.remove_surface(&root, surface);
        self.presentation_requested = true;
        self.pending_surface_events
            .push(HostSurfaceEvent::TreeSnapshot {
                surface: surface_id,
                snapshot,
            });
    }
}

impl XdgShellHandler for ServerState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let Some(id) = allocate_surface_id(&mut self.next_surface_id) else {
            warn!("refused an xdg-toplevel because SurfaceId space is exhausted");
            surface.send_close();
            return;
        };
        surface.with_pending_state(|state| {
            state.size = Some(Size::<i32, Logical>::from((CLIENT_WIDTH, CLIENT_HEIGHT)));
        });
        self.output.enter(surface.wl_surface());
        send_preferred_surface_scale(&self.output, surface.wl_surface());
        let rejection_surface = surface.clone();
        let state = ToplevelState {
            surface,
            tree: SurfaceTreeState::default(),
        };
        if !self.toplevels.insert(id, state) {
            warn!(?id, "refused a duplicate xdg-toplevel registration");
            rejection_surface.send_close();
            return;
        }
        self.pending_surface_events
            .push(HostSurfaceEvent::Created { surface: id });
        info!(surface_id = id.raw(), "created a nested xdg-toplevel");
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {
        debug!("ignoring an xdg-popup in the initial slice");
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface();
        let Some((id, _state)) = self.toplevels.remove_surface(wl_surface) else {
            return;
        };
        self.clear_input_focus_for_surface(wl_surface, self.event_time());
        self.output.leave(wl_surface);
        if self.focused_toplevel == Some(id) {
            self.focused_toplevel = None;
        }
        self.pending_surface_events
            .push(HostSurfaceEvent::Destroyed { surface: id });
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
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: Mode) {
        stage_server_side_decoration(&toplevel);
        if toplevel.is_initial_configure_sent() {
            // Respond even when the client requested client-side decorations and the
            // compositor's server-side mode therefore did not change.
            toplevel.send_configure();
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        stage_server_side_decoration(&toplevel);
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
    }
}

impl OutputHandler for ServerState {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_store_keeps_multiple_values_and_removes_only_the_target() {
        let mut store = IndexedStore::<u32, &'static str>::default();
        let first = SurfaceId::new(1);
        let second = SurfaceId::new(2);
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
        let first = SurfaceId::new(1);
        assert!(store.insert(first, 10, "first"));
        assert!(!store.insert(first, 20, "duplicate id"));
        assert!(!store.insert(SurfaceId::new(2), 10, "duplicate key"));
    }

    #[test]
    fn surface_ids_exhaust_without_wrapping() {
        let mut next = Some(u64::MAX);
        assert_eq!(
            allocate_surface_id(&mut next),
            Some(SurfaceId::new(u64::MAX))
        );
        assert_eq!(next, None);
        assert_eq!(allocate_surface_id(&mut next), None);
    }
}
