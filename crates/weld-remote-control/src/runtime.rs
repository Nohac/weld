//! Native-thread remote-control endpoint over the shared relay and composed view.

use crate::{
    Capture, CaptureJob, Command, ControlRegistry, GrantState, InputCoordinates, InputReceipt,
    InputStage, Operation, Pending, PixelScale, RemoteControlClient, Reply, Snapshot, Viewport,
};
use anyhow::{Context, Result, ensure};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
    sync::{Arc, Mutex, atomic::Ordering, mpsc},
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
    RawScrollFrame, RawScrollPhase, RawScrollSource, SurfaceBufferChange, SurfaceStreamMode,
    ToplevelLayout,
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
    checkpoints: HashMap<ClientSurfaceId, VecDeque<(u64, Option<u64>)>>,
    sequence: u64,
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
                            if let Some(grant) = state.grants.get(&event.surface)
                                && let Ok(mut viewport) = grant.viewport.lock()
                            {
                                let next =
                                    commit
                                        .buffers
                                        .iter()
                                        .find_map(|update| match &update.change {
                                            SurfaceBufferChange::Replaced { buffer, .. } => buffer
                                                .access::<ComposedBuffer>()
                                                .map(|scene| Viewport {
                                                    logical_width: scene.logical_size.width,
                                                    logical_height: scene.logical_size.height,
                                                    pixel_width: scene.extent.width,
                                                    pixel_height: scene.extent.height,
                                                    input_coordinates:
                                                        InputCoordinates::WindowLocalLogical,
                                                    pixel_scale: PixelScale::for_frame(scene),
                                                }),
                                            _ => None,
                                        });
                                if next.is_some() || !commit.mapped {
                                    *viewport = next;
                                }
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
                state.checkpoints.remove(&surface);
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
        let mut state = self.0.borrow_mut();
        for pending in std::mem::take(&mut state.accepted) {
            let result = if pending.grant.permit.is_authorized()
                && pending.grant.input_epoch.load(Ordering::Acquire) == pending.input_epoch
            {
                if let Some(sequence) = state.sequence.checked_add(1) {
                    state.sequence = sequence;
                    let revision = state
                        .frames
                        .get(&pending.grant.surface)
                        .map(|frame| frame.revision.raw());
                    let checkpoints = state.checkpoints.entry(pending.grant.surface).or_default();
                    if checkpoints.len() == 128 {
                        checkpoints.pop_front();
                    }
                    checkpoints.push_back((sequence, revision));
                    Ok(Reply::Input(InputReceipt {
                        sequence: sequence.to_string(),
                        stage: InputStage::EffectsQueued,
                        frame_revision: revision.map(|revision| revision.to_string()),
                    }))
                } else {
                    Err("input sequences exhausted".into())
                }
            } else {
                Err("window control revoked or input ownership changed".into())
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
        state.checkpoints.clear();
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

enum Execution {
    Effects,
    Capture(Box<dyn CaptureJob>, Snapshot),
    Wait,
}

struct Adapter {
    source: ClientSourceId,
    upstream: ClientSourceId,
    relay: SourceRelayAdapter,
    state: Rc<RefCell<State>>,
    requests: mpsc::Receiver<Command>,
    capture: Box<dyn Capture>,
    capturing: Vec<Capturing>,
    waiting: Vec<Pending>,
    positions: HashMap<ClientSurfaceId, InputPosition>,
    held_keys: HashMap<ClientSurfaceId, HashSet<u32>>,
    text_surfaces: HashSet<ClientSurfaceId>,
    started: Instant,
    input_epochs: HashMap<ClientSurfaceId, u64>,
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
        checkpoints: HashMap::new(),
        sequence: 0,
    }));
    let adapter = Adapter {
        source,
        upstream,
        relay: SourceRelayAdapter::new(upstream, Port(state.clone())),
        state,
        requests,
        capture: Box::new(capture),
        capturing: Vec::new(),
        waiting: Vec::new(),
        positions: HashMap::new(),
        held_keys: HashMap::new(),
        text_surfaces: HashSet::new(),
        started: Instant::now(),
        input_epochs: HashMap::new(),
    };
    (
        ClientAdapterRegistration::new(
            ClientSourceDescriptor::new(source, ClientProvenance::Relocated),
            adapter,
            PassthroughClientImporter,
        ),
        RemoteControlClient {
            sender,
            grants: Arc::new(Mutex::new(ControlRegistry::default())),
            wake: Arc::new(wake),
        },
    )
}

impl Adapter {
    fn synchronize_input_ownership(&mut self) {
        let mut state = self.state.borrow_mut();
        for (surface, grant) in &state.grants {
            let epoch = grant.input_epoch.load(Ordering::Acquire);
            if self
                .input_epochs
                .insert(*surface, epoch)
                .is_some_and(|previous| previous != epoch)
            {
                self.relay
                    .release_input(|member| state.views.root(member) == Some(*surface));
                self.positions.remove(surface);
                self.held_keys.remove(surface);
            }
        }
        for (surface, epoch) in &self.input_epochs {
            if !epoch.is_multiple_of(2) {
                state.input.remove(*surface);
            }
        }
    }

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
            self.held_keys.remove(&source);
            self.input_epochs.remove(&source);
        }
    }

    fn execute(&mut self, pending: &Pending) -> Result<Execution> {
        let epoch = pending.grant.input_epoch.load(Ordering::Acquire);
        ensure!(
            pending.operation.is_capture()
                || (epoch.is_multiple_of(2) && epoch == pending.input_epoch),
            "local interaction interrupted remote input"
        );
        ensure!(
            pending
                .grant
                .capabilities
                .allows(pending.operation.capability()),
            "window capability is disabled"
        );
        ensure!(
            pending.grant.permit.is_authorized(),
            "window control revoked"
        );
        ensure!(
            pending.deadline > Instant::now(),
            "operation timed out before completion"
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
        match pending.operation.clone() {
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
                let held = self.held_keys.entry(surface).or_default();
                if pressed {
                    held.insert(keycode);
                } else {
                    held.remove(&keycode);
                }
            }
            Operation::Chord { keycodes } => {
                ensure!(
                    !keycodes.is_empty() && keycodes.len() <= 16,
                    "a chord needs 1 to 16 keycodes"
                );
                let mut unique = HashSet::new();
                for key in &keycodes {
                    ensure!(
                        *key > 0 && *key <= 767 && unique.insert(*key),
                        "invalid or duplicate chord keycode"
                    );
                    ensure!(
                        !self
                            .held_keys
                            .get(&surface)
                            .is_some_and(|held| held.contains(key)),
                        "chord key is already held; release it first"
                    );
                }
                messages.push(focus);
                for (key, state) in keycodes
                    .iter()
                    .map(|key| (key, KeyboardKeyState::Pressed))
                    .chain(
                        keycodes
                            .iter()
                            .rev()
                            .map(|key| (key, KeyboardKeyState::Released)),
                    )
                {
                    messages.push(event(
                        ClientInputTarget::Keyboard { surface },
                        InputEventKind::Keyboard {
                            keycode: LinuxKeycode(*key),
                            state,
                        },
                    ));
                }
            }
            Operation::TypeText { text } => {
                let text = weld_client::TextInput::try_from(text).map_err(anyhow::Error::msg)?;
                ensure!(
                    self.text_surfaces.contains(&surface),
                    "whole-text input requires a native Wayland window; XWayland needs a separate text delivery backend"
                );
                ensure!(
                    self.held_keys.values().all(HashSet::is_empty),
                    "release held remote keys before typing text"
                );
                messages.push(focus);
                messages.push(event(
                    ClientInputTarget::Keyboard { surface },
                    InputEventKind::Text { text },
                ));
            }
            Operation::Scroll {
                x,
                y,
                horizontal,
                vertical,
                wheel,
            } => {
                let position = valid_position(x, y)?;
                ensure!(
                    horizontal.is_finite()
                        && vertical.is_finite()
                        && horizontal.abs() <= 1_000_000.
                        && vertical.abs() <= 1_000_000.,
                    "invalid scroll delta"
                );
                self.positions.insert(surface, position);
                messages.push(event(
                    pointer,
                    InputEventKind::PointerMotion {
                        position,
                        relative: None,
                    },
                ));
                let axis = RawScrollFrame {
                    source: if wheel {
                        RawScrollSource::Wheel
                    } else {
                        RawScrollSource::Finger
                    },
                    phase: RawScrollPhase::Moved,
                    horizontal: horizontal * if wheel { 15. } else { 1. },
                    vertical: vertical * if wheel { 15. } else { 1. },
                    horizontal_v120: wheel.then_some((horizontal * 120.).round() as i32),
                    vertical_v120: wheel.then_some((vertical * 120.).round() as i32),
                    horizontal_stop: false,
                    vertical_stop: false,
                };
                messages.push(event(
                    pointer,
                    InputEventKind::PointerAxis {
                        position: Some(position),
                        axis,
                    },
                ));
                if !wheel {
                    messages.push(event(
                        pointer,
                        InputEventKind::PointerAxis {
                            position: Some(position),
                            axis: RawScrollFrame {
                                phase: RawScrollPhase::Ended,
                                horizontal: 0.,
                                vertical: 0.,
                                horizontal_stop: horizontal != 0.,
                                vertical_stop: vertical != 0.,
                                ..axis
                            },
                        },
                    ));
                }
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
            Operation::Scale { factor } => {
                let scale_120 = factor
                    .map(|factor| {
                        let units = (factor * 120.).round();
                        ensure!(
                            factor.is_finite()
                                && factor > 0.
                                && (1.0..=f64::from(u32::MAX)).contains(&units),
                            "scale must be positive and representable in 1/120 units"
                        );
                        Ok(units as u32)
                    })
                    .transpose()?;
                messages.push(DestinationEnvelope {
                    session,
                    message: DestinationMessage::Request(ClientRequest::Surface(
                        ClientSurfaceRequest {
                            surface,
                            kind: ClientSurfaceRequestKind::SetPreferredScale { scale_120 },
                        },
                    )),
                });
            }
            Operation::Capture | Operation::CaptureAfter { .. } => {
                let after_sequence =
                    if let Operation::CaptureAfter { sequence, .. } = &pending.operation {
                        let sequence_number =
                            sequence.parse::<u64>().context("invalid input sequence")?;
                        let (_, revision) = state
                            .checkpoints
                            .get(&surface)
                            .and_then(|checkpoints| {
                                checkpoints
                                    .iter()
                                    .find(|(candidate, _)| *candidate == sequence_number)
                            })
                            .context("input checkpoint expired or belongs to another window")?;
                        if revision.is_some_and(|revision| frame.revision.raw() <= revision) {
                            return Ok(Execution::Wait);
                        }
                        Some(sequence.clone())
                    } else {
                        None
                    };
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
                return Ok(Execution::Capture(
                    job,
                    Snapshot {
                        width: scene.extent.width,
                        height: scene.extent.height,
                        logical_width: scene.logical_size.width,
                        logical_height: scene.logical_size.height,
                        revision: frame.revision.raw().to_string(),
                        after_sequence,
                        input_coordinates: InputCoordinates::WindowLocalLogical,
                        pixel_scale: PixelScale::for_frame(scene),
                        png_base64: String::new(),
                    },
                ));
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
        Ok(Execution::Effects)
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
            .chain(self.waiting.iter().map(|pending| pending.deadline))
            .chain(self.relay.next_deadline())
            .min()
    }
    fn drain_events(&mut self, events: &mut ClientEventQueue) {
        self.retire();
        self.synchronize_input_ownership();
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
                    self.state.borrow_mut().checkpoints.remove(&surface);
                    let session = HoistSessionId::new(grant.permit.id().0);
                    self.input_epochs
                        .insert(surface, grant.input_epoch.load(Ordering::Acquire));
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
                Command::Operation(pending)
                    if pending.operation.is_capture()
                        && self.waiting.len() + self.capturing.len() >= 4 =>
                {
                    let _ = pending.reply.send(Err("window capture busy".into()));
                }
                Command::Operation(pending) => match self.execute(&pending) {
                    Ok(Execution::Capture(job, snapshot)) => self.capturing.push(Capturing {
                        pending,
                        job,
                        snapshot,
                        replied: false,
                    }),
                    Ok(Execution::Effects) => self.state.borrow_mut().accepted.push(pending),
                    Ok(Execution::Wait) => {
                        if self.waiting.len() + self.capturing.len() < 4 {
                            self.waiting.push(pending);
                        } else {
                            let _ = pending.reply.send(Err("window capture busy".into()));
                        }
                    }
                    Err(error) => {
                        let _ = pending.reply.send(Err(error.to_string()));
                    }
                },
            }
        }
        self.relay.drain_events(events);
        for pending in std::mem::take(&mut self.waiting) {
            match self.execute(&pending) {
                Ok(Execution::Capture(job, snapshot)) => self.capturing.push(Capturing {
                    pending,
                    job,
                    snapshot,
                    replied: false,
                }),
                Ok(Execution::Wait) => self.waiting.push(pending),
                Ok(Execution::Effects) => {
                    let _ = pending
                        .reply
                        .send(Err("capture produced input effects".into()));
                }
                Err(error) => {
                    let _ = pending.reply.send(Err(error.to_string()));
                }
            }
        }
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
        self.synchronize_input_ownership();
        self.relay.drain_effects(effects);
        for (surface, grant) in &self.state.borrow().grants {
            if let Some(epoch) = self.input_epochs.get(surface) {
                grant.input_epoch_applied.store(*epoch, Ordering::Release);
            }
        }
    }
    fn drain_presentation_claims(&mut self, updates: &mut Vec<ClientPresentationUpdate>) {
        self.retire();
        self.relay.drain_presentation_claims(updates);
    }
    fn observe_event(&mut self, event: &ClientSurfaceEvent) {
        match &event.kind {
            ClientSurfaceEventKind::Metadata(metadata) => {
                if metadata.x11_class().is_none() {
                    self.text_surfaces.insert(event.surface);
                } else {
                    self.text_surfaces.remove(&event.surface);
                }
            }
            ClientSurfaceEventKind::Destroyed => {
                self.text_surfaces.remove(&event.surface);
            }
            _ => {}
        }
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
