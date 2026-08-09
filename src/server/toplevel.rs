//! XDG toplevel registration, lifecycle, commits, and surface indexing.

use std::{collections::HashMap, hash::Hash};

use smithay::{
    reexports::wayland_server::{
        Client, Resource,
        backend::ObjectId,
        protocol::{wl_buffer, wl_seat, wl_surface::WlSurface},
    },
    utils::{Logical, Serial, Size},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, with_states,
        },
        fractional_scale::FractionalScaleHandler,
        output::OutputHandler,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        shm::{ShmHandler, ShmState},
    },
};
use tracing::{debug, info, warn};

use crate::surface::{HostSurfaceEvent, SurfaceContentView, SurfaceFrame, SurfaceId};

use super::{
    ClientState, ServerState,
    output::send_preferred_surface_scale,
    shm::{SurfaceBufferMetadata, checked_buffer_scale, copy_shm_buffer, surface_content_view},
};

const CLIENT_WIDTH: i32 = 640;
const CLIENT_HEIGHT: i32 = 480;

pub(super) struct ToplevelState {
    pub(super) surface: ToplevelSurface,
    buffer: Option<SurfaceBufferMetadata>,
    view: Option<SurfaceContentView>,
    /// Whether the client currently has a buffer attached, even if Weld could
    /// not import that buffer into the composition.
    mapped: bool,
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
        for toplevel in self.toplevels.values() {
            if toplevel.surface.alive() {
                send_preferred_surface_scale(&self.output, toplevel.surface.wl_surface());
            }
        }
    }

    pub(super) fn complete_surface_presentation(&mut self) {
        let time = self.event_time();
        for toplevel in self
            .toplevels
            .values()
            .filter(|toplevel| toplevel.mapped && toplevel.surface.alive())
        {
            with_states(toplevel.surface.wl_surface(), |states| {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                for callback in attributes.current().frame_callbacks.drain(..) {
                    callback.done(time);
                }
            });
        }
    }

    fn handle_root_commit(&mut self, surface_id: SurfaceId, surface: &WlSurface) {
        let Some((retained_buffer, previous_view)) = self
            .toplevels
            .get(surface_id)
            .map(|toplevel| (toplevel.buffer, toplevel.view))
        else {
            return;
        };
        let (assignment, buffer_scale, buffer_transform) = with_states(surface, |states| {
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            let current = attributes.current();
            (
                current.buffer.take(),
                current.buffer_scale,
                current.buffer_transform,
            )
        });

        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let copied = copy_shm_buffer(&buffer).and_then(|copied| {
                    let metadata = SurfaceBufferMetadata {
                        width: copied.width,
                        height: copied.height,
                        scale: checked_buffer_scale(buffer_scale)?,
                        transform: buffer_transform,
                    };
                    let view = surface_content_view(surface, metadata)?;
                    Ok((copied, metadata, view))
                });
                buffer.release();
                match copied {
                    Ok((copied, metadata, view)) => {
                        if let Some(toplevel) = self.toplevels.get_mut(surface_id) {
                            toplevel.buffer = Some(metadata);
                            toplevel.view = Some(view);
                            toplevel.mapped = true;
                        }
                        self.pending_surface_events.push(HostSurfaceEvent::Frame {
                            surface: surface_id,
                            frame: SurfaceFrame {
                                width: copied.width,
                                height: copied.height,
                                view,
                                bgra_pixels: copied.bgra_pixels,
                                opaque: copied.opaque,
                            },
                        });
                    }
                    Err(error) => {
                        if let Some(toplevel) = self.toplevels.get_mut(surface_id) {
                            toplevel.buffer = None;
                            toplevel.view = None;
                            toplevel.mapped = true;
                        }
                        self.clear_input_focus_for_surface(surface, self.event_time());
                        self.pending_surface_events
                            .push(HostSurfaceEvent::Unmapped {
                                surface: surface_id,
                            });
                        warn!(%error, ?surface_id, "could not display a mapped client buffer");
                    }
                }
            }
            Some(BufferAssignment::Removed) => {
                if let Some(toplevel) = self.toplevels.get_mut(surface_id) {
                    toplevel.buffer = None;
                    toplevel.view = None;
                    toplevel.mapped = false;
                }
                self.clear_input_focus_for_surface(surface, self.event_time());
                self.pending_surface_events
                    .push(HostSurfaceEvent::Unmapped {
                        surface: surface_id,
                    });
            }
            None => {
                let Some(retained_buffer) = retained_buffer else {
                    return;
                };
                let metadata = match checked_buffer_scale(buffer_scale) {
                    Ok(scale) => SurfaceBufferMetadata {
                        scale,
                        transform: buffer_transform,
                        ..retained_buffer
                    },
                    Err(error) => {
                        warn!(%error, ?surface_id, "ignored an invalid client surface scale");
                        return;
                    }
                };
                match surface_content_view(surface, metadata) {
                    Ok(view) if previous_view != Some(view) => {
                        if let Some(toplevel) = self.toplevels.get_mut(surface_id) {
                            toplevel.buffer = Some(metadata);
                            toplevel.view = Some(view);
                        }
                        self.pending_surface_events
                            .push(HostSurfaceEvent::ViewChanged {
                                surface: surface_id,
                                view,
                            });
                    }
                    Ok(_) => {
                        if let Some(toplevel) = self.toplevels.get_mut(surface_id) {
                            toplevel.buffer = Some(metadata);
                        }
                    }
                    Err(error) => {
                        warn!(%error, ?surface_id, "ignored an invalid client surface view");
                    }
                }
            }
        }
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

    fn commit(&mut self, surface: &WlSurface) {
        let Some(surface_id) = self.toplevels.id_for_surface(surface) else {
            debug!(surface = ?surface.id(), "ignoring a non-root surface commit");
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
        self.handle_root_commit(surface_id, surface);
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
            buffer: None,
            view: None,
            mapped: false,
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
