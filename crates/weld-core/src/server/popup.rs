//! XDG popup registration, committed geometry, surface trees, and explicit grabs.

use smithay::{
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy,
        find_popup_root_surface,
    },
    input::{Seat, pointer::Focus},
    reexports::wayland_server::{
        Resource,
        backend::ObjectId,
        protocol::{wl_seat, wl_surface::WlSurface},
    },
    utils::Serial,
    wayland::shell::xdg::{PopupSurface, PositionerState},
};
use tracing::{debug, info, warn};

use crate::surface::{LogicalPoint, PopupDescriptor, SurfaceId};

use super::{
    PendingSurfaceEvent, PendingSurfaceEventKind, ServerState,
    output::send_preferred_surface_scale,
    surface_tree::SurfaceTreeState,
    toplevel::{IndexedStore, allocate_surface_id},
};

pub(super) struct PopupState {
    pub(super) surface: PopupSurface,
    pub(super) tree: SurfaceTreeState,
    published: Option<PopupDescriptor>,
}

#[derive(Default)]
pub(super) struct PopupStore(IndexedStore<ObjectId, PopupState>);

impl PopupStore {
    fn insert(&mut self, id: SurfaceId, state: PopupState) -> bool {
        let object_id = state.surface.wl_surface().id();
        self.0.insert(id, object_id, state)
    }

    pub(super) fn get(&self, id: SurfaceId) -> Option<&PopupState> {
        self.0.get(id)
    }

    pub(super) fn get_mut(&mut self, id: SurfaceId) -> Option<&mut PopupState> {
        self.0.get_mut(id)
    }

    pub(super) fn id_for_surface(&self, surface: &WlSurface) -> Option<SurfaceId> {
        self.0.id_for_key(&surface.id())
    }

    fn remove_surface(&mut self, surface: &WlSurface) -> Option<(SurfaceId, PopupState)> {
        self.0.remove_by_key(&surface.id())
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &PopupState> {
        self.0.values()
    }
}

impl ServerState {
    pub(super) fn register_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let Some(id) = allocate_surface_id(&mut self.next_surface_id) else {
            warn!("refused an xdg-popup because SurfaceId space is exhausted");
            surface.send_popup_done();
            return;
        };
        let kind = PopupKind::Xdg(surface.clone());
        if let Err(error) = self.popup_manager.track_popup(kind) {
            warn!(%error, "refused an xdg-popup that could not be tracked");
            surface.send_popup_done();
            return;
        }

        self.output.enter(surface.wl_surface());
        send_preferred_surface_scale(&self.output, surface.wl_surface());
        let rejection_surface = surface.clone();
        if !self.popups.insert(
            id,
            PopupState {
                surface,
                tree: SurfaceTreeState::default(),
                published: None,
            },
        ) {
            warn!(?id, "refused a duplicate xdg-popup registration");
            rejection_surface.send_popup_done();
            return;
        }
        info!(surface_id = id.raw(), "created a nested xdg-popup");
    }

    /// Handle a commit whose subsurface root has the xdg-popup role.
    pub(super) fn commit_popup(&mut self, root: &WlSurface) -> bool {
        let Some(surface_id) = self.popups.id_for_surface(root) else {
            return false;
        };
        self.popup_manager.commit(root);

        let Some(popup) = self.popups.get(surface_id) else {
            return true;
        };
        if !popup.surface.is_initial_configure_sent() {
            if let Err(error) = popup.surface.send_configure() {
                warn!(%error, ?surface_id, "failed to send an initial xdg-popup configure");
                popup.surface.send_popup_done();
            }
            return true;
        }

        let snapshot = {
            let (popups, releases) = (&mut self.popups, &mut self.dmabuf_releases);
            let Some(popup) = popups.get_mut(surface_id) else {
                return true;
            };
            popup.tree.update(surface_id, root, releases)
        };
        if snapshot.root.is_none() {
            self.clear_input_focus_for_surface(root, self.event_time());
        }

        // PopupManager traverses its own cached popup state. Keep this outside
        // SurfaceTreeState::update so neither traversal can recursively lock a
        // surface's MultiCache entry.
        self.publish_popup_layout(root);
        self.presentation_requested = true;
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: surface_id,
            kind: PendingSurfaceEventKind::TreeSnapshot(snapshot),
        });
        true
    }

    pub(super) fn remove_popup_surface(&mut self, root: &WlSurface, removed: &WlSurface) -> bool {
        let Some(surface_id) = self.popups.id_for_surface(root) else {
            return false;
        };
        self.clear_input_focus_for_surface(removed, self.event_time());
        let Some(popup) = self.popups.get_mut(surface_id) else {
            return true;
        };
        let snapshot = popup.tree.remove_surface(root, removed);
        self.presentation_requested = true;
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: surface_id,
            kind: PendingSurfaceEventKind::TreeSnapshot(snapshot),
        });
        true
    }

    pub(super) fn destroy_popup(&mut self, surface: PopupSurface) {
        let wl_surface = surface.wl_surface();
        let Some((id, _state)) = self.popups.remove_surface(wl_surface) else {
            return;
        };
        self.clear_input_focus_for_surface(wl_surface, self.event_time());
        self.output.leave(wl_surface);
        self.presentation_requested = true;
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: id,
            kind: PendingSurfaceEventKind::Destroyed,
        });
    }

    pub(super) fn reposition_popup(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }

    pub(super) fn begin_popup_grab(
        &mut self,
        surface: PopupSurface,
        seat_resource: wl_seat::WlSeat,
        serial: Serial,
    ) {
        let Some(seat) = Seat::<Self>::from_resource(&seat_resource) else {
            warn!("ignored an xdg-popup grab from an unknown seat");
            return;
        };
        if seat != self.seat {
            warn!("ignored an xdg-popup grab from a different seat");
            return;
        }

        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            debug!("ignored an xdg-popup grab without a live toplevel root");
            return;
        };
        if self.toplevels.id_for_surface(&root).is_none() {
            debug!("ignored an xdg-popup grab whose root is not a Weld toplevel");
            return;
        }

        // PopupManager contains an internal root equality assertion. Supplying
        // the exact root returned by find_popup_root_surface keeps that
        // invariant protocol-derived instead of relying on Weld's index.
        let Ok(mut grab) = self.popup_manager.grab_popup(root, kind, &seat, serial) else {
            return;
        };
        let accepted_serial = grab.previous_serial().unwrap_or(serial);

        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial) || keyboard.has_grab(accepted_serial))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial) || pointer.has_grab(accepted_serial))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
        self.ordinary_implicit_grab = None;
        self.popup_grab = Some(grab);
    }

    pub(super) fn dismiss_popup_grab(&mut self) {
        if let Some(mut grab) = self.popup_grab.take() {
            grab.ungrab(PopupUngrabStrategy::All);
        }
    }

    fn publish_popup_layout(&mut self, popup_root: &WlSurface) {
        let Some(kind) = self.popup_manager.find_popup(popup_root) else {
            return;
        };
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let Some(owner) = self.toplevels.id_for_surface(&root) else {
            return;
        };
        // Smithay returns topmost-first. Bevy's local ZIndex increases toward
        // the front, so assign the first item the highest rank.
        let layouts = PopupManager::popups_for_surface(&root).collect::<Vec<_>>();
        let count = layouts.len();
        let layouts = layouts
            .into_iter()
            .enumerate()
            .filter_map(|(index, (popup, location))| {
                let surface = self.popups.id_for_surface(popup.wl_surface())?;
                let stack_index = i32::try_from(count.saturating_sub(index)).unwrap_or(i32::MAX);
                Some((
                    surface,
                    PopupDescriptor {
                        owner,
                        position: LogicalPoint::new(location.x as f32, location.y as f32),
                        stack_index,
                    },
                ))
            })
            .collect::<Vec<_>>();
        for (surface, popup) in layouts {
            let Some(state) = self.popups.get_mut(surface) else {
                continue;
            };
            if state.published == Some(popup) {
                continue;
            }
            state.published = Some(popup);
            self.pending_surface_events.push_back(PendingSurfaceEvent {
                surface,
                kind: PendingSurfaceEventKind::PopupConfigured(popup),
            });
        }
    }
}
