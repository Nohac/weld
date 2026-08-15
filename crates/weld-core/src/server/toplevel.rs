//! XDG toplevel registration, lifecycle, commits, and surface indexing.

use std::{collections::HashMap, hash::Hash};

use smithay::{
    reexports::{
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode, shell::server::xdg_toplevel,
        },
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
            add_blocker, add_pre_commit_hook, get_role, with_states,
        },
        dmabuf::get_dmabuf,
        drm_syncobj::DrmSyncobjCachedState,
        fractional_scale::FractionalScaleHandler,
        output::OutputHandler,
        shell::xdg::{
            PopupSurface, PositionerState, SurfaceCachedState as XdgSurfaceCachedState,
            ToplevelSurface, XdgShellHandler, XdgShellState, decoration::XdgDecorationHandler,
        },
        shm::{ShmHandler, ShmState},
    },
};
use tracing::{debug, info, warn};

use crate::surface::{Extent, SurfaceId, WindowDecoration, WindowResizeEdge};

use super::{
    ClientState, PendingSurfaceEvent, PendingSurfaceEventKind, ServerState,
    output::send_preferred_surface_scale,
    surface_tree::{
        SurfaceTreeState, collect_surfaces, owning_root, release_untracked_surface_tree,
    },
};

const CLIENT_WIDTH: i32 = 640;
const CLIENT_HEIGHT: i32 = 480;

pub(super) struct ToplevelState {
    pub(super) surface: ToplevelSurface,
    pub(super) decoration: WindowDecoration,
    pub(super) tree: SurfaceTreeState,
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

    pub(super) fn resize_toplevel(&mut self, surface: SurfaceId, requested: Extent) {
        let changed = self.stage_toplevel_size(surface, requested);
        let Some(toplevel) = self.toplevels.get(surface) else {
            return;
        };
        if changed && toplevel.surface.is_initial_configure_sent() {
            toplevel.surface.send_pending_configure();
        }
    }

    pub(super) fn stage_toplevel_size(&self, surface: SurfaceId, requested: Extent) -> bool {
        let Some(toplevel) = self.toplevels.get(surface) else {
            warn!(?surface, "ignored a resize request for an unknown surface");
            return false;
        };
        if !toplevel.surface.alive() {
            return false;
        }
        let constraints = with_states(toplevel.surface.wl_surface(), |states| {
            let mut cached = states.cached_state.get::<XdgSurfaceCachedState>();
            let current = cached.current();
            (current.min_size, current.max_size)
        });
        let requested_width = i32::try_from(requested.width.max(1)).unwrap_or(i32::MAX);
        let requested_height = i32::try_from(requested.height.max(1)).unwrap_or(i32::MAX);
        let size = Size::<i32, Logical>::from((
            constrain_dimension(requested_width, constraints.0.w, constraints.1.w),
            constrain_dimension(requested_height, constraints.0.h, constraints.1.h),
        ));
        toplevel.surface.with_pending_state(|state| {
            if state.size == Some(size) {
                false
            } else {
                state.size = Some(size);
                true
            }
        })
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
            kind: PendingSurfaceEventKind::DecorationChanged {
                decoration: WindowDecoration::ServerSide,
            },
        });
    }

    pub(super) fn send_all_surface_scales(&self) {
        let surfaces = self
            .toplevels
            .values()
            .filter(|toplevel| toplevel.surface.alive())
            .flat_map(|toplevel| collect_surfaces(toplevel.surface.wl_surface()))
            .chain(
                self.popups
                    .values()
                    .filter(|popup| popup.surface.alive())
                    .flat_map(|popup| collect_surfaces(popup.surface.wl_surface())),
            )
            .filter(Resource::is_alive)
            .collect::<Vec<_>>();
        for surface in surfaces {
            send_preferred_surface_scale(&self.output, &surface);
        }
    }

    pub(crate) fn stage_frame_callbacks(&mut self) -> u64 {
        self.presentation_requested = false;
        let presentation_id = self.next_presentation_id;
        self.next_presentation_id = self.next_presentation_id.saturating_add(1);
        let surfaces = self
            .toplevels
            .values()
            .filter(|toplevel| {
                let root = toplevel.surface.wl_surface();
                toplevel.surface.alive() && toplevel.tree.client_mapped(root)
            })
            .flat_map(|toplevel| collect_surfaces(toplevel.surface.wl_surface()))
            .chain(
                self.popups
                    .values()
                    .filter(|popup| {
                        let root = popup.surface.wl_surface();
                        popup.surface.alive() && popup.tree.client_mapped(root)
                    })
                    .flat_map(|popup| collect_surfaces(popup.surface.wl_surface())),
            )
            .filter(Resource::is_alive)
            .collect::<Vec<_>>();
        let mut callbacks = Vec::new();
        for surface in surfaces {
            with_states(&surface, |states| {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                callbacks.append(&mut attributes.current().frame_callbacks);
            });
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
            for callback in callbacks {
                callback.done(time);
            }
        }
    }

    fn update_surface_tree(&mut self, surface_id: SurfaceId, root: &WlSurface) {
        let (toplevels, releases) = (&mut self.toplevels, &mut self.dmabuf_releases);
        let Some(toplevel) = toplevels.get_mut(surface_id) else {
            return;
        };
        let snapshot = toplevel.tree.update(surface_id, root, releases);
        if snapshot.root.is_none() {
            self.clear_input_focus_for_surface(root, self.event_time());
        }
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: surface_id,
            kind: PendingSurfaceEventKind::TreeSnapshot(snapshot),
        });
    }
}

impl BufferHandler for ServerState {
    fn buffer_destroyed(&mut self, buffer: &wl_buffer::WlBuffer) {
        if let Ok(dmabuf) = get_dmabuf(buffer) {
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

    fn new_subsurface(&mut self, surface: &WlSurface, _parent: &WlSurface) {
        self.output.enter(surface);
        send_preferred_surface_scale(&self.output, surface);
    }

    fn commit(&mut self, surface: &WlSurface) {
        if self.commit_cursor_surface(surface) {
            return;
        }
        if !SurfaceTreeState::should_process_commit(surface) {
            return;
        }
        let root = owning_root(surface);
        let Some(surface_id) = self.toplevels.id_for_surface(&root) else {
            if !self.commit_popup(&root) {
                if get_role(&root).is_some() {
                    release_untracked_surface_tree(&root);
                }
                debug!(surface = ?surface.id(), "ignoring a surface outside a tracked xdg surface tree");
            }
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
        self.remove_cursor_surface(surface);
        self.output.leave(surface);
        let root = owning_root(surface);
        let Some(surface_id) = self.toplevels.id_for_surface(&root) else {
            self.remove_popup_surface(&root, surface);
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
            decoration: WindowDecoration::ClientSide,
            tree: SurfaceTreeState::default(),
        };
        if !self.toplevels.insert(id, state) {
            warn!(?id, "refused a duplicate xdg-toplevel registration");
            rejection_surface.send_close();
            return;
        }
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: id,
            kind: PendingSurfaceEventKind::Created {
                decoration: WindowDecoration::ClientSide,
            },
        });
        info!(surface_id = id.raw(), "created a nested xdg-toplevel");
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        self.register_popup(surface, positioner);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        self.begin_pointer_move(surface, seat, serial);
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
        let wl_surface = surface.wl_surface();
        let Some((id, _state)) = self.toplevels.remove_surface(wl_surface) else {
            return;
        };
        self.clear_input_focus_for_surface(wl_surface, self.event_time());
        self.output.leave(wl_surface);
        if self.focused_toplevel == Some(id) {
            self.focused_toplevel = None;
        }
        self.pending_resizes.discard(id);
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: id,
            kind: PendingSurfaceEventKind::Destroyed,
        });
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        self.destroy_popup(surface);
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

fn constrain_dimension(requested: i32, minimum: i32, maximum: i32) -> i32 {
    let minimum = minimum.max(1);
    let maximum = if maximum > 0 {
        maximum.max(minimum)
    } else {
        i32::MAX
    };
    requested.clamp(minimum, maximum)
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

    #[test]
    fn resize_dimensions_follow_committed_client_constraints() {
        assert_eq!(constrain_dimension(10, 20, 100), 20);
        assert_eq!(constrain_dimension(60, 20, 100), 60);
        assert_eq!(constrain_dimension(120, 20, 100), 100);
        assert_eq!(constrain_dimension(120, 20, 0), 120);
    }
}
