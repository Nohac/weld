//! Session-lock ownership, protected-output acknowledgement, and locker input.

mod pointer;

use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use smithay::{
    backend::input::{InputTime, KeyEvent, KeyState, Keycode},
    output::Output,
    reexports::{
        wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1,
        wayland_server::{
            DisplayHandle, Resource,
            protocol::{wl_output::WlOutput, wl_surface::WlSurface},
        },
    },
    utils::SERIAL_COUNTER,
    wayland::{
        compositor::{SurfaceAttributes, with_states},
        session_lock::{LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker},
    },
};

use super::{
    PendingSurfaceBufferContent, PendingSurfaceTreeSnapshot, ServerState,
    surface_tree::{SurfaceTreeState, collect_surfaces, owning_root},
    toplevel::{allocate_surface_id, client_id_for_surface},
};
use crate::{
    OutputId,
    input::{KeyboardKeyState, RawSeatEvent, RawSeatEventKind},
    surface::{SurfaceId, SurfaceLayerId, SurfaceLayerPlacement},
};

pub(crate) struct LockImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

pub(crate) struct LockLayer {
    pub placement: SurfaceLayerPlacement,
    pub image: Rc<LockImage>,
}

pub(crate) struct LockFrame {
    pub logical_width: f32,
    pub logical_height: f32,
    pub layers: Vec<LockLayer>,
}

struct LockOutput {
    surface: LockSurface,
    id: SurfaceId,
    tree: SurfaceTreeState,
    images: HashMap<SurfaceLayerId, Rc<LockImage>>,
    frame: Option<Rc<LockFrame>>,
}

impl LockOutput {
    fn update(&mut self, snapshot: PendingSurfaceTreeSnapshot) {
        self.images
            .retain(|layer, _| snapshot.buffers.iter().any(|buffer| buffer.layer == *layer));
        for buffer in snapshot.buffers {
            match buffer.content {
                PendingSurfaceBufferContent::ShmPixels(pixels) => {
                    self.images.insert(
                        buffer.layer,
                        Rc::new(LockImage {
                            width: buffer.width,
                            height: buffer.height,
                            pixels,
                        }),
                    );
                }
                PendingSurfaceBufferContent::Retained => {}
                PendingSurfaceBufferContent::ImportedDmabuf(_) => {
                    self.images.remove(&buffer.layer);
                }
            }
        }
        self.frame = snapshot
            .root
            .filter(|_| snapshot.client_mapped)
            .map(|root| {
                Rc::new(LockFrame {
                    logical_width: root.view.logical_width,
                    logical_height: root.view.logical_height,
                    layers: std::iter::once(root)
                        .chain(snapshot.overlays)
                        .filter_map(|placement| {
                            Some(LockLayer {
                                image: self.images.get(&placement.layer)?.clone(),
                                placement,
                            })
                        })
                        .collect(),
                })
            });
    }
}

/// A generation identifies the pixels queued by one accepted locking request.
/// Old desktop page flips and abandoned lockers cannot confirm a later lock.
#[derive(Default)]
struct Protection {
    generation: u64,
    pending: HashSet<OutputId>,
}

impl Protection {
    fn begin(&mut self, outputs: impl Iterator<Item = OutputId>) -> Option<u64> {
        self.generation = self.generation.checked_add(1)?;
        self.pending = outputs.collect();
        Some(self.generation)
    }

    fn presented(&mut self, output: OutputId, generation: u64) -> bool {
        generation == self.generation && self.pending.remove(&output) && self.pending.is_empty()
    }
}

pub(super) struct SessionLock {
    protocol: SessionLockManagerState,
    enabled: Arc<AtomicBool>,
    owner: Option<ExtSessionLockV1>,
    confirmation: Option<SessionLocker>,
    protection: Protection,
    outputs: HashMap<OutputId, LockOutput>,
    redraw: bool,
    orphan_reported: bool,
    restore_focus: bool,
}

impl SessionLock {
    pub(super) fn new(display: &DisplayHandle) -> Self {
        let enabled = Arc::new(AtomicBool::new(false));
        let visible = enabled.clone();
        Self {
            protocol: SessionLockManagerState::new::<ServerState, _>(display, move |_| {
                visible.load(Ordering::Acquire)
            }),
            enabled,
            owner: None,
            confirmation: None,
            protection: Protection::default(),
            outputs: HashMap::new(),
            redraw: false,
            orphan_reported: false,
            restore_focus: false,
        }
    }
}

impl ServerState {
    /// Install only after a native presenter capable of replacing every output exists.
    pub(crate) fn enable_session_lock(&mut self) {
        self.session_lock.enabled.store(true, Ordering::Release);
    }

    pub(crate) fn session_locked(&self) -> bool {
        self.session_lock.owner.is_some()
    }

    pub(crate) fn lock_generation(&self) -> Option<u64> {
        self.session_locked()
            .then_some(self.session_lock.protection.generation)
    }

    pub(crate) fn lock_images(&self) -> HashMap<OutputId, Rc<LockFrame>> {
        self.session_lock
            .outputs
            .iter()
            .filter_map(|(output, content)| {
                content
                    .surface
                    .ext_session_lock()
                    .is_alive()
                    .then_some(())?;
                content.surface.alive().then_some(())?;
                Some((*output, content.frame.clone()?))
            })
            .collect()
    }

    pub(crate) fn take_lock_redraw(&mut self) -> bool {
        if self
            .session_lock
            .owner
            .as_ref()
            .is_some_and(|owner| !owner.is_alive())
            && !self.session_lock.orphan_reported
        {
            self.session_lock.orphan_reported = true;
            self.session_lock.outputs.clear();
            self.session_lock.redraw = true;
            tracing::warn!("session locker disconnected; retaining protected outputs");
        }
        std::mem::take(&mut self.session_lock.redraw)
    }

    pub(crate) fn take_lock_focus_restore(&mut self) -> bool {
        std::mem::take(&mut self.session_lock.restore_focus)
    }

    pub(super) fn configure_lock_output(&mut self, id: OutputId) {
        let Some(content) = self.session_lock.outputs.get(&id) else {
            return;
        };
        let Some(output) = self.outputs.get(&id) else {
            return;
        };
        let size = output
            .metrics
            .mode()
            .size
            .to_f64()
            .to_logical(output.metrics.scale_factor())
            .to_i32_round::<i32>();
        content
            .surface
            .with_pending_state(|state| state.size = Some((size.w as u32, size.h as u32).into()));
        super::output::send_preferred_surface_scale(&output.native, content.surface.wl_surface());
        content.surface.send_configure();
        self.session_lock.redraw = true;
    }

    pub(crate) fn lock_frame_presented(&mut self, output: OutputId, generation: Option<u64>) {
        let Some(generation) =
            generation.filter(|generation| Some(*generation) == self.lock_generation())
        else {
            return;
        };
        if self.session_lock.protection.presented(output, generation)
            && let Some(confirmation) = self.session_lock.confirmation.take()
        {
            confirmation.lock();
            tracing::info!("session locked after protected presentation on every output");
        }
        if let Some(lock) = self.session_lock.outputs.get(&output) {
            let time = self.event_time();
            for surface in collect_surfaces(lock.surface.wl_surface()) {
                with_states(&surface, |states| {
                    for callback in states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .frame_callbacks
                        .drain(..)
                    {
                        callback.done(time);
                    }
                });
            }
        }
    }

    pub(super) fn commit_lock_surface(&mut self, surface: &WlSurface) -> bool {
        let Some(content) = self
            .session_lock
            .outputs
            .values_mut()
            .find(|content| content.surface.wl_surface() == surface)
        else {
            return false;
        };
        let snapshot = content
            .tree
            .update(content.id, surface, &mut self.dmabuf_releases, None);
        content.update(snapshot);
        self.session_lock.redraw = true;
        true
    }

    pub(super) fn remove_lock_surface(&mut self, surface: &WlSurface) -> bool {
        let root = owning_root(surface);
        let Some((&output, content)) = self
            .session_lock
            .outputs
            .iter_mut()
            .find(|(_, content)| content.surface.wl_surface() == &root)
        else {
            return false;
        };
        if surface == &root {
            self.session_lock.outputs.remove(&output);
        } else {
            let snapshot = content.tree.remove_surface(&root, surface);
            content.update(snapshot);
        }
        self.session_lock.redraw = true;
        true
    }

    /// Consume local events before shell shortcuts, hit testing, and adapter routing.
    pub(crate) fn dispatch_lock_input(&mut self, event: &RawSeatEvent) -> bool {
        if !self.session_locked() {
            return false;
        }
        if matches!(event.event, RawSeatEventKind::HostFocusLost) {
            self.release_host_input(event.time);
            return true;
        }
        let RawSeatEventKind::Keyboard { keycode, state, .. } = event.event else {
            self.dispatch_lock_pointer(event);
            return true;
        };
        let Some(surface) = self
            .session_lock
            .outputs
            .get(&self.primary_output)
            .or_else(|| self.session_lock.outputs.values().next())
            .filter(|content| {
                content.surface.alive() && content.surface.ext_session_lock().is_alive()
            })
            .map(|content| content.surface.wl_surface().clone())
        else {
            return true;
        };
        let Some(client) = surface.client() else {
            return true;
        };
        let global = self.local_input.native.clone();
        let Some(seat) = self.bind_input_seat(&client, &global) else {
            return true;
        };
        let Some(keyboard) = seat.get_keyboard() else {
            return true;
        };
        let Some(code) = keycode.0.checked_add(8).map(Keycode::new) else {
            return true;
        };
        let key = match state {
            KeyboardKeyState::Pressed => KeyEvent::Pressed,
            KeyboardKeyState::Released => KeyEvent::Released,
            KeyboardKeyState::Repeated => return true,
        };
        keyboard.set_focus(self, Some(surface.into()), SERIAL_COUNTER.next_serial());
        let (_, modifiers_changed) = keyboard.input_intercept(
            self,
            code,
            if state == KeyboardKeyState::Pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
            |_, _, _| (),
        );
        self.apply_local_modifiers(&keyboard);
        keyboard.input_forward(
            self,
            code,
            key,
            SERIAL_COUNTER.next_serial(),
            InputTime::from_millis(event.time),
            modifiers_changed,
        );
        true
    }
}

impl SessionLockHandler for ServerState {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock.protocol
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        if self
            .session_lock
            .owner
            .as_ref()
            .is_some_and(Resource::is_alive)
        {
            return;
        }
        if self
            .session_lock
            .protection
            .begin(self.outputs.keys().copied())
            .is_none()
        {
            return;
        }
        self.release_host_input(self.event_time());
        self.stop_output_captures();
        self.session_lock.owner = Some(confirmation.ext_session_lock().clone());
        self.session_lock.confirmation = Some(confirmation);
        self.session_lock.outputs.clear();
        self.session_lock.orphan_reported = false;
        self.session_lock.redraw = true;
        tracing::info!("session lock requested");
    }

    fn unlock(&mut self) {
        self.release_host_input(self.event_time());
        self.session_lock.owner = None;
        self.session_lock.confirmation = None;
        self.session_lock.outputs.clear();
        self.session_lock.redraw = true;
        self.session_lock.restore_focus = true;
        tracing::info!("session unlocked");
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        if self.session_lock.owner.as_ref() != Some(surface.ext_session_lock()) {
            return;
        }
        let Some(native) = Output::from_resource(&output) else {
            return;
        };
        let Some((&id, output)) = self
            .outputs
            .iter()
            .find(|(_, output)| output.native == native)
        else {
            return;
        };
        let size = output
            .metrics
            .mode()
            .size
            .to_f64()
            .to_logical(output.metrics.scale_factor())
            .to_i32_round::<i32>();
        surface
            .with_pending_state(|state| state.size = Some((size.w as u32, size.h as u32).into()));
        output.native.enter(surface.wl_surface());
        super::output::send_preferred_surface_scale(&output.native, surface.wl_surface());
        let Some(client) = client_id_for_surface(surface.wl_surface()) else {
            return;
        };
        let Some(surface_id) = allocate_surface_id(&mut self.next_surface_id, client) else {
            return;
        };
        self.session_lock.outputs.insert(
            id,
            LockOutput {
                surface,
                id: surface_id,
                tree: SurfaceTreeState::shm_only(),
                images: HashMap::new(),
                frame: None,
            },
        );
        self.session_lock.redraw = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_waits_for_every_output_and_ignores_old_generations() {
        let a = OutputId::new(1);
        let b = OutputId::new(2);
        let mut protection = Protection::default();
        let first = protection.begin([a, b].into_iter()).expect("generation");
        assert!(!protection.presented(a, first));
        assert!(!protection.presented(a, first));
        let replacement = protection.begin([a, b].into_iter()).expect("generation");
        assert!(!protection.presented(b, first));
        assert!(!protection.presented(b, replacement));
        assert!(protection.presented(a, replacement));
        assert!(!protection.presented(a, replacement));
    }
}
