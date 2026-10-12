//! Native-thread remote-control endpoint over the shared relay and composed view.

use crate::{
    Capture, CaptureJob, Command, GrantState, Operation, Pending, RemoteControlClient, Reply,
    Snapshot,
};
use anyhow::{Context, Result, ensure};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::Instant,
};
use weld_client::{
    ButtonState, ClientAdapter, ClientAdapterCommandEnvelope, ClientAdapterEffect,
    ClientAdapterRegistration, ClientBufferId, ClientCursorUpdate, ClientEventQueue,
    ClientFocusRequest, ClientInputEvent, ClientInputTarget, ClientPresentationUpdate,
    ClientProvenance, ClientRequest, ClientSourceDescriptor, ClientSourceId, ClientSurfaceCommit,
    ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceId, ClientSurfaceRequest,
    ClientSurfaceRequestKind, ComposedBuffer, Extent, InputEventKind, InputPosition,
    KeyboardKeyState, LinuxButtonCode, LinuxKeycode, PassthroughClientImporter, PresentationRate,
    SurfaceBufferChange, SurfaceStreamMode, ToplevelLayout,
};
use weld_hoist_core::{
    HoistEndpointCommand, HoistPortResult, HoistSessionId, HoistSourcePort, SourcePortCommand,
    SourceRelayAdapter, view::SourceViews, view_input::ViewInput,
};
use weld_hoist_protocol::{DestinationEnvelope, DestinationMessage};

struct State {
    grants: HashMap<ClientSurfaceId, Arc<GrantState>>,
    views: SourceViews,
    input: ViewInput,
    frames: HashMap<ClientSurfaceId, ClientSurfaceCommit>,
    messages: Vec<DestinationEnvelope>,
    accepted: Vec<Pending>,
}

struct Port(Rc<RefCell<State>>);

impl HoistSourcePort for Port {
    fn submit(&mut self, command: SourcePortCommand) -> HoistPortResult<()> {
        let mut state = self.0.borrow_mut();
        match command {
            SourcePortCommand::Surface { session, event } => {
                for (_, event) in state.views.observe(session, event)? {
                    match event.kind {
                        ClientSurfaceEventKind::Commit(commit) => {
                            if !commit.mapped
                                && let Some(grant) = state.grants.get(&event.surface)
                            {
                                grant.permit.end();
                            }
                            state.frames.insert(event.surface, commit);
                        }
                        ClientSurfaceEventKind::Destroyed => {
                            state.frames.remove(&event.surface);
                            if let Some(grant) = state.grants.get(&event.surface) {
                                grant.permit.end();
                            }
                        }
                        _ => {}
                    }
                }
            }
            SourcePortCommand::Cursor {
                session,
                update,
                sequence,
            } => state.messages.push(DestinationEnvelope {
                session,
                message: DestinationMessage::CursorReceived {
                    surface: update.surface,
                    sequence,
                },
            }),
            SourcePortCommand::WithdrawSurface { surface, .. } => {
                state.frames.remove(&surface);
                if let Some((_, event)) = state.views.withdraw(surface)?
                    && let ClientSurfaceEventKind::Commit(commit) = event.kind
                {
                    state.frames.insert(event.surface, commit);
                }
                state.input.remove(surface);
                if let Some(grant) = state.grants.remove(&surface) {
                    grant.permit.end();
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn poll(&mut self) -> HoistPortResult<Vec<DestinationEnvelope>> {
        Ok(std::mem::take(&mut self.0.borrow_mut().messages))
    }
    fn accept_destination(&mut self, _: &DestinationEnvelope) -> HoistPortResult<()> {
        Ok(())
    }
    fn progress_after_destination(&mut self) -> HoistPortResult<()> {
        Ok(())
    }
    fn effects_drained(&mut self) {
        for pending in self.0.borrow_mut().accepted.drain(..) {
            let result = if pending.grant.permit.is_authorized() {
                Ok(Reply::Applied)
            } else {
                Err("window control revoked".into())
            };
            let _ = pending.reply.send(result);
        }
    }
    fn disconnect(&mut self) {
        let mut state = self.0.borrow_mut();
        for grant in state.grants.values() {
            grant.permit.end();
        }
        state.frames.clear();
        state.views = SourceViews::new(SurfaceStreamMode::Composited);
        state.messages.clear();
        for pending in state.accepted.drain(..) {
            let _ = pending
                .reply
                .send(Err("remote control relay disconnected".into()));
        }
    }
}

struct Capturing {
    pending: Pending,
    job: Box<dyn CaptureJob>,
    snapshot: Snapshot,
    replied: bool,
}

struct Adapter {
    source: ClientSourceId,
    upstream: ClientSourceId,
    relay: SourceRelayAdapter,
    state: Rc<RefCell<State>>,
    requests: mpsc::Receiver<Command>,
    capture: Box<dyn Capture>,
    capturing: Vec<Capturing>,
    positions: HashMap<ClientSurfaceId, InputPosition>,
    started: Instant,
}

pub fn registration(
    upstream: ClientSourceId,
    source: ClientSourceId,
    wake: impl Fn() + Send + Sync + 'static,
    capture: impl Capture + 'static,
) -> (ClientAdapterRegistration, RemoteControlClient) {
    let (sender, requests) = mpsc::sync_channel(64);
    let state = Rc::new(RefCell::new(State {
        grants: HashMap::new(),
        views: SourceViews::new(SurfaceStreamMode::Composited),
        input: ViewInput::default(),
        frames: HashMap::new(),
        messages: Vec::new(),
        accepted: Vec::new(),
    }));
    let adapter = Adapter {
        source,
        upstream,
        relay: SourceRelayAdapter::new(upstream, Port(state.clone())),
        state,
        requests,
        capture: Box::new(capture),
        capturing: Vec::new(),
        positions: HashMap::new(),
        started: Instant::now(),
    };
    (
        ClientAdapterRegistration::new(
            ClientSourceDescriptor::new(source, ClientProvenance::Relocated),
            adapter,
            PassthroughClientImporter,
        ),
        RemoteControlClient {
            sender,
            grants: Arc::new(Mutex::new(HashMap::new())),
            wake: Arc::new(wake),
        },
    )
}

impl Adapter {
    fn retire(&mut self) {
        let retired: Vec<_> = self
            .state
            .borrow()
            .grants
            .iter()
            .filter(|(_, grant)| !grant.permit.is_authorized())
            .map(|(surface, _)| *surface)
            .collect();
        for source in retired {
            self.relay.apply_command(ClientAdapterCommandEnvelope::new(
                self.source,
                HoistEndpointCommand::Unmap { source },
            ));
            self.positions.remove(&source);
        }
    }

    fn execute(&mut self, pending: &Pending) -> Result<Option<(Box<dyn CaptureJob>, Snapshot)>> {
        ensure!(
            pending
                .grant
                .capabilities
                .allows(pending.operation.capability()),
            "window capability is disabled"
        );
        ensure!(
            pending.deadline > Instant::now() && pending.grant.permit.is_authorized(),
            "window control expired or revoked"
        );
        let surface = pending.grant.surface;
        let mut state = self.state.borrow_mut();
        ensure!(
            state
                .grants
                .get(&surface)
                .is_some_and(|grant| Arc::ptr_eq(grant, &pending.grant)),
            "stale window control"
        );
        let frame = state
            .frames
            .get(&surface)
            .context("window has no ready frame")?;
        let root = frame
            .root
            .filter(|_| frame.mapped)
            .context("window is not mapped")?;
        let logical = root.view;
        let session = HoistSessionId::new(pending.grant.permit.id().0);
        let time = self.started.elapsed().as_millis() as u32;
        let pointer = ClientInputTarget::Pointer {
            surface,
            layer: root.layer,
        };
        let event = |target, event| DestinationEnvelope {
            session,
            message: DestinationMessage::input(ClientInputEvent {
                target,
                host_position: None,
                event,
                time,
            }),
        };
        let mut messages = Vec::new();
        let valid_position = |x: f64, y: f64| -> Result<InputPosition> {
            ensure!(
                x.is_finite()
                    && y.is_finite()
                    && x >= 0.0
                    && y >= 0.0
                    && x < f64::from(logical.logical_width)
                    && y < f64::from(logical.logical_height),
                "pointer outside current window viewport"
            );
            Ok(InputPosition::new(x, y))
        };
        let focus = DestinationEnvelope {
            session,
            message: DestinationMessage::Request(ClientRequest::Focus(ClientFocusRequest {
                source: self.upstream,
                surface: Some(surface),
            })),
        };
        match pending.operation {
            Operation::Pointer { x, y } => {
                let position = valid_position(x, y)?;
                self.positions.insert(surface, position);
                messages.push(event(
                    pointer,
                    InputEventKind::PointerMotion {
                        position,
                        relative: None,
                    },
                ));
            }
            Operation::Click { x, y, button } => {
                ensure!((272..=279).contains(&button), "invalid pointer button");
                let position = valid_position(x, y)?;
                self.positions.insert(surface, position);
                messages.push(focus);
                messages.push(event(
                    pointer,
                    InputEventKind::PointerMotion {
                        position,
                        relative: None,
                    },
                ));
                for pressed in [true, false] {
                    messages.push(event(
                        pointer,
                        InputEventKind::PointerButton {
                            position: Some(position),
                            button: LinuxButtonCode(button),
                            state: if pressed {
                                ButtonState::Pressed
                            } else {
                                ButtonState::Released
                            },
                        },
                    ));
                }
            }
            Operation::Button { button, pressed } => {
                ensure!((272..=279).contains(&button), "invalid pointer button");
                let position = self
                    .positions
                    .get(&surface)
                    .copied()
                    .context("move the pointer before pressing a button")?;
                if pressed {
                    messages.push(focus);
                }
                messages.push(event(
                    pointer,
                    InputEventKind::PointerButton {
                        position: Some(position),
                        button: LinuxButtonCode(button),
                        state: if pressed {
                            ButtonState::Pressed
                        } else {
                            ButtonState::Released
                        },
                    },
                ));
            }
            Operation::Key { keycode, pressed } => {
                ensure!(keycode > 0 && keycode <= 767, "invalid Linux keycode");
                if pressed {
                    messages.push(focus);
                }
                messages.push(event(
                    ClientInputTarget::Keyboard { surface },
                    InputEventKind::Keyboard {
                        keycode: LinuxKeycode(keycode),
                        state: if pressed {
                            KeyboardKeyState::Pressed
                        } else {
                            KeyboardKeyState::Released
                        },
                    },
                ));
            }
            Operation::Resize { width, height } => {
                ensure!(
                    width > 0
                        && height > 0
                        && width <= i32::MAX as u32
                        && height <= i32::MAX as u32,
                    "invalid window size"
                );
                messages.push(DestinationEnvelope {
                    session,
                    message: DestinationMessage::Request(ClientRequest::Surface(
                        ClientSurfaceRequest {
                            surface,
                            kind: ClientSurfaceRequestKind::Configure {
                                logical_size: Extent::new(width, height),
                                layout: ToplevelLayout::Tiled,
                                resizing: false,
                                fullscreen: false,
                            },
                        },
                    )),
                });
            }
            Operation::Capture => {
                ensure!(self.capturing.len() < 4, "window capture busy");
                let scene = frame
                    .buffers
                    .iter()
                    .find_map(|update| match &update.change {
                        SurfaceBufferChange::Replaced { buffer, .. } => {
                            buffer.access::<ComposedBuffer>()
                        }
                        _ => None,
                    })
                    .context("composed window frame unavailable")?;
                let job = self.capture.begin(scene)?;
                return Ok(Some((
                    job,
                    Snapshot {
                        width: scene.extent.width,
                        height: scene.extent.height,
                        logical_width: scene.logical_size.width,
                        logical_height: scene.logical_size.height,
                        revision: frame.revision.raw().to_string(),
                        png_base64: String::new(),
                    },
                )));
            }
        }
        for message in messages {
            let State {
                input,
                views,
                messages,
                ..
            } = &mut *state;
            messages.extend(input.route(views, message));
        }
        Ok(None)
    }
}

impl ClientAdapter for Adapter {
    fn input_source(&self) -> Option<ClientSourceId> {
        Some(self.upstream)
    }
    fn presentation_source(&self) -> Option<ClientSourceId> {
        Some(self.upstream)
    }
    fn next_deadline(&self) -> Option<Instant> {
        self.capturing
            .iter()
            .filter(|capture| !capture.replied)
            .map(|capture| capture.pending.deadline)
            .chain(self.relay.next_deadline())
            .min()
    }
    fn drain_events(&mut self, events: &mut ClientEventQueue) {
        self.retire();
        for _ in 0..64 {
            let Ok(command) = self.requests.try_recv() else {
                break;
            };
            match command {
                Command::Grant(grant) => {
                    if !grant.permit.is_authorized() {
                        continue;
                    }
                    let surface = grant.surface;
                    let session = HoistSessionId::new(grant.permit.id().0);
                    self.state.borrow_mut().grants.insert(surface, grant);
                    self.relay.apply_command(ClientAdapterCommandEnvelope::new(
                        self.source,
                        HoistEndpointCommand::Map {
                            session,
                            source: surface,
                        },
                    ));
                    self.state.borrow_mut().messages.push(DestinationEnvelope {
                        session,
                        message: DestinationMessage::Request(ClientRequest::Surface(
                            ClientSurfaceRequest {
                                surface,
                                kind: ClientSurfaceRequestKind::SetPresentation {
                                    rate: Some(PresentationRate::HZ_60),
                                },
                            },
                        )),
                    });
                }
                Command::Operation(pending) => match self.execute(&pending) {
                    Ok(Some((job, snapshot))) => self.capturing.push(Capturing {
                        pending,
                        job,
                        snapshot,
                        replied: false,
                    }),
                    Ok(None) => self.state.borrow_mut().accepted.push(pending),
                    Err(error) => {
                        let _ = pending.reply.send(Err(error.to_string()));
                    }
                },
            }
        }
        self.relay.drain_events(events);
        let mut index = 0;
        while index < self.capturing.len() {
            let capture = &mut self.capturing[index];
            if !capture.replied
                && (!capture.pending.grant.permit.is_authorized()
                    || capture.pending.deadline <= Instant::now())
            {
                let _ = capture
                    .pending
                    .reply
                    .send(Err("window capture revoked or expired".into()));
                capture.replied = true;
            }
            let result = capture.job.poll();
            if let Some(result) = result {
                let mut capture = self.capturing.swap_remove(index);
                if !capture.replied {
                    let result = result.map(|png| {
                        capture.snapshot.png_base64 = png;
                        Reply::Snapshot(capture.snapshot)
                    });
                    let _ = capture
                        .pending
                        .reply
                        .send(result.map_err(|error| error.to_string()));
                }
            } else {
                index += 1;
            }
        }
    }
    fn drain_effects(&mut self, effects: &mut Vec<ClientAdapterEffect>) {
        self.retire();
        self.relay.drain_effects(effects);
    }
    fn drain_presentation_claims(&mut self, updates: &mut Vec<ClientPresentationUpdate>) {
        self.retire();
        self.relay.drain_presentation_claims(updates);
    }
    fn observe_event(&mut self, event: &ClientSurfaceEvent) {
        self.relay.observe_event(event);
    }
    fn observe_cursor_update(&mut self, update: &ClientCursorUpdate) {
        self.relay.observe_cursor_update(update);
    }
    fn observe_retired_buffer(&mut self, buffer: ClientBufferId) {
        self.relay.observe_retired_buffer(buffer);
    }
    fn apply_request(&mut self, _: ClientRequest) {}
    fn apply_input(&mut self, _: ClientInputEvent) {}
    fn apply_command(&mut self, _: ClientAdapterCommandEnvelope) {}
    fn host_focus_lost(&mut self, _: u32) {}
}

#[cfg(test)]
mod tests;
