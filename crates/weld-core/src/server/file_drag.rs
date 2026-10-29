//! Compositor-originated local file drag, using the desktop's shared data-device seat.

mod transfer;

use super::ServerState;
use crate::{input::SurfaceHit, runtime::drag::FileDragPayload};
use anyhow::{Result, ensure};
use smithay::{
    backend::input::{ButtonState as NativeButtonState, InputTime},
    input::{
        dnd::{DnDGrab, DndAction, Source, SourceMetadata},
        pointer::{ButtonEvent, Focus, GrabStartData, MotionEvent},
    },
    utils::{IsAlive, SERIAL_COUNTER},
};
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use transfer::TransferWorker;
use weld_client::{
    ClientInputEvent, ClientInputTarget, ClientPointerRoute, ClientPointerRouteUpdate,
    InputEventKind, InputPosition, KeyboardKeyState, LinuxButtonCode, LinuxKeycode,
};
use weld_input::{ButtonState, RawSeatEvent, RawSeatEventKind};
use winit::keyboard::{Key, NamedKey};

#[derive(Default)]
pub(super) struct FileDragState {
    active: Option<ActiveDrag>,
    worker: Option<TransferWorker>,
    route: Option<ClientPointerRoute>,
    position: Option<InputPosition>,
    physical_buttons: HashSet<LinuxButtonCode>,
    consumed_buttons: HashSet<LinuxButtonCode>,
    escape: Option<LinuxKeycode>,
}

struct ActiveDrag {
    source: FileSource,
    deadline: Instant,
}

#[derive(Clone)]
struct FileSource {
    payload: FileDragPayload,
    transfers: std::sync::mpsc::SyncSender<transfer::Command>,
    alive: Arc<AtomicBool>,
    dragging: Arc<AtomicBool>,
}
impl IsAlive for FileSource {
    fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}
impl Source for FileSource {
    fn metadata(&self) -> Option<SourceMetadata> {
        Some(SourceMetadata {
            mime_types: vec!["text/uri-list".into()],
            dnd_actions: [DndAction::Copy].into_iter().collect(),
        })
    }
    fn choose_action(&self, _: DndAction) {}
    fn send(&self, mime: &str, fd: std::os::fd::OwnedFd) {
        if self.alive()
            && mime == "text/uri-list"
            && self
                .transfers
                .try_send(transfer::Command::Write(transfer::Transfer {
                    fd,
                    bytes: self.payload.0.clone(),
                    alive: self.alive.clone(),
                }))
                .is_err()
        {
            tracing::warn!("file drag transfer capacity reached");
        }
    }
    fn drop_performed(&self) {
        self.dragging.store(false, Ordering::Release);
    }
    fn cancel(&self) {
        self.alive.store(false, Ordering::Release);
        self.dragging.store(false, Ordering::Release);
    }
    fn finished(&self) {
        self.alive.store(false, Ordering::Release);
    }
}

impl ServerState {
    pub(crate) fn begin_file_drag(&mut self, payload: FileDragPayload) -> Result<()> {
        self.poll_file_drag();
        ensure!(!self.session_locked(), "session is locked");
        ensure!(
            self.file_drag.active.is_none(),
            "a file drag is already active"
        );
        let position = self.file_drag.position.ok_or_else(|| {
            anyhow::anyhow!("move the local pointer into the desktop before starting a drag")
        })?;
        ensure!(
            self.file_drag.physical_buttons.is_empty(),
            "release pointer buttons before starting a file drag"
        );
        ensure!(
            !self
                .input_seats()
                .any(|input| input.controller.get().is_none()
                    && (input
                        .native
                        .get_pointer()
                        .is_some_and(|pointer| pointer.is_grabbed())
                        || input.touch_count.get() != 0)),
            "local input is busy with another grab"
        );
        let seat = self.local_input.native.clone();
        let pointer = seat
            .get_pointer()
            .ok_or_else(|| anyhow::anyhow!("local pointer unavailable"))?;
        if self.file_drag.worker.is_none() {
            self.file_drag.worker = Some(TransferWorker::new()?);
        }
        let worker = self
            .file_drag
            .worker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("file transfer worker unavailable"))?;
        let source = FileSource {
            payload,
            transfers: worker.sender.clone(),
            alive: Arc::new(AtomicBool::new(true)),
            dragging: Arc::new(AtomicBool::new(true)),
        };
        let grab = DnDGrab::new_pointer(
            &self.display_handle,
            GrabStartData {
                focus: None,
                button: 0x110,
                location: (position.x, position.y).into(),
            },
            source.clone(),
            seat,
        );
        let previous = self.local_input.pointer_binding.borrow().clone();
        if let Some(previous) = previous {
            self.clear_seat_pointer(&previous, self.event_time());
        }
        self.file_drag.active = Some(ActiveDrag {
            source,
            deadline: Instant::now() + Duration::from_secs(60),
        });
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
        self.file_drag_motion(self.event_time());
        self.presentation_requested = true;
        Ok(())
    }

    pub(crate) fn file_drag_active(&self) -> bool {
        self.file_drag.active.is_some()
    }

    pub(crate) fn file_drag_timeout(&self, now: Instant) -> Option<Duration> {
        self.file_drag
            .active
            .as_ref()
            .map(|active| active.deadline.saturating_duration_since(now))
    }

    pub(crate) fn cancel_file_drag(&mut self) {
        if let Some(active) = self.file_drag.active.take() {
            active.source.cancel();
            if let Some(pointer) = self.local_input.native.get_pointer() {
                pointer.unset_grab(
                    self,
                    SERIAL_COUNTER.next_serial(),
                    InputTime::from_millis(self.event_time()),
                );
                if self
                    .file_drag
                    .consumed_buttons
                    .contains(&LinuxButtonCode(0x110))
                {
                    pointer.button(
                        self,
                        &ButtonEvent {
                            serial: SERIAL_COUNTER.next_serial(),
                            time: InputTime::from_millis(self.event_time()),
                            button: 0x110,
                            state: NativeButtonState::Released,
                        },
                    );
                    pointer.frame(self);
                }
            }
            self.presentation_requested = true;
            self.restore_file_drag_pointer();
        }
    }

    pub(super) fn poll_file_drag(&mut self) {
        if self
            .file_drag
            .active
            .as_ref()
            .is_some_and(|active| Instant::now() >= active.deadline)
        {
            self.cancel_file_drag();
        } else if self
            .file_drag
            .active
            .as_ref()
            .is_some_and(|active| !active.source.dragging.load(Ordering::Acquire))
        {
            self.file_drag.active = None;
            self.presentation_requested = true;
            self.restore_file_drag_pointer();
        }
    }

    fn restore_file_drag_pointer(&mut self) {
        if self.session_locked() {
            return;
        }
        if let (Some(route), Some(position)) = (self.file_drag.route, self.file_drag.position) {
            self.route_controller_input(
                None,
                ClientInputEvent {
                    target: ClientInputTarget::Pointer {
                        surface: route.surface,
                        layer: route.layer,
                    },
                    host_position: Some(position),
                    event: InputEventKind::PointerMotion {
                        position: route.transform.transform(position),
                        relative: None,
                    },
                    time: self.event_time(),
                },
            );
        }
    }

    pub(crate) fn publish_file_drag_route(&mut self, update: ClientPointerRouteUpdate) {
        self.file_drag.route = update
            .route
            .filter(|route| route.surface.source() == crate::WAYLAND_CLIENT_SOURCE);
        if self.file_drag_active() {
            self.file_drag_motion(update.time);
        }
    }

    fn file_drag_motion(&mut self, time: u32) {
        let Some(position) = self.file_drag.position else {
            return;
        };
        let Some(pointer) = self.local_input.native.get_pointer() else {
            return;
        };
        let hit = self.file_drag.route.map(|route| SurfaceHit {
            surface: route.surface,
            layer: route.layer,
            local_position: route.transform.transform(position),
        });
        let focus = self.pointer_focus(position, hit);
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: (position.x, position.y).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::from_millis(time),
            },
        );
        pointer.frame(self);
    }

    /// Buttons belong to the native drag. Motion continues through shell hit
    /// testing, whose route publication corrects the native offer target.
    pub(crate) fn dispatch_file_drag_input(&mut self, event: &RawSeatEvent) -> bool {
        match &event.event {
            RawSeatEventKind::HostFocusLost => {
                self.file_drag.position = None;
                self.file_drag.route = None;
                self.cancel_file_drag();
                self.file_drag.physical_buttons.clear();
                self.file_drag.consumed_buttons.clear();
                self.file_drag.escape = None;
            }
            RawSeatEventKind::PointerMotion { position, .. } => {
                self.file_drag.position = Some(*position);
                if self.file_drag_active() {
                    self.file_drag_motion(event.time);
                }
            }
            RawSeatEventKind::PointerLeft { .. } => {
                self.file_drag.route = None;
                if self.file_drag_active() {
                    self.file_drag_motion(event.time);
                }
                self.file_drag.position = None;
            }
            RawSeatEventKind::PointerButton {
                button,
                state,
                position,
            } => {
                if let Some(position) = position {
                    self.file_drag.position = Some(*position);
                }
                if *state == ButtonState::Pressed {
                    self.file_drag.physical_buttons.insert(*button);
                } else {
                    self.file_drag.physical_buttons.remove(button);
                }
                let consumed_release = *state == ButtonState::Released
                    && self.file_drag.consumed_buttons.remove(button);
                if self.file_drag_active() {
                    self.file_drag_motion(event.time);
                    if *state == ButtonState::Pressed {
                        self.file_drag.consumed_buttons.insert(*button);
                    }
                    if button.0 != 0x110 {
                        if *state == ButtonState::Pressed {
                            self.cancel_file_drag();
                        }
                        return true;
                    }
                    if let Some(pointer) = self.local_input.native.get_pointer() {
                        pointer.button(
                            self,
                            &ButtonEvent {
                                serial: SERIAL_COUNTER.next_serial(),
                                time: InputTime::from_millis(event.time),
                                button: button.0,
                                state: if *state == ButtonState::Pressed {
                                    NativeButtonState::Pressed
                                } else {
                                    NativeButtonState::Released
                                },
                            },
                        );
                        pointer.frame(self);
                    }
                    self.poll_file_drag();
                    return true;
                }
                return consumed_release;
            }
            RawSeatEventKind::Keyboard {
                keycode,
                logical_key,
                state,
            } => {
                if self.file_drag.escape == Some(*keycode) {
                    if *state == KeyboardKeyState::Released {
                        self.file_drag.escape = None;
                    }
                    return true;
                }
                if self.file_drag_active()
                    && *state == KeyboardKeyState::Pressed
                    && matches!(logical_key, Some(Key::Named(NamedKey::Escape)))
                {
                    self.file_drag.escape = Some(*keycode);
                    self.cancel_file_drag();
                    return true;
                }
            }
            RawSeatEventKind::PointerAxis { .. } | RawSeatEventKind::PointerGesture { .. } => {
                return self.file_drag_active();
            }
        }
        false
    }
}
