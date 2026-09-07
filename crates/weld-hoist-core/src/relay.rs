//! Shared source and destination relay policy over binding-owned ports.

use std::{
    collections::{HashMap, VecDeque},
    error::Error,
    fmt::Display,
    time::{Duration, Instant},
};

use weld_client::{
    ClientAdapter, ClientAdapterCommandEnvelope, ClientAdapterEffect, ClientBufferId,
    ClientEventQueue, ClientInputEvent, ClientInputTarget, ClientRequest, ClientRouteAliasUpdate,
    ClientSourceDescriptor, ClientSourceId, ClientSurfaceCommit, ClientSurfaceEvent,
    ClientSurfaceEventKind, ClientSurfaceId, ClientSurfaceRequestKind, InputEventKind,
    InputPosition, LinuxButtonCode, LinuxKeycode, PointerGestureKind, RawScrollPhase,
    RawScrollSource,
};
use weld_hoist_protocol::{DestinationEnvelope, DestinationMessage, HoistSessionId};

use crate::{HoistEndpointCommand, relocated_surface};

pub type HoistPortError = Box<dyn Error + Send + Sync>;
pub type HoistPortResult<T> = Result<T, HoistPortError>;

pub enum SourcePortCommand {
    MapSurface {
        session: HoistSessionId,
        surface: ClientSurfaceId,
    },
    Surface {
        session: HoistSessionId,
        event: ClientSurfaceEvent,
    },
    Cursor {
        session: HoistSessionId,
        update: weld_client::ClientCursorUpdate,
        sequence: u64,
    },
    WithdrawSurface {
        session: HoistSessionId,
        surface: ClientSurfaceId,
    },
    RetireUpstreamBuffer(ClientBufferId),
}

/// Source-side binding boundary.
///
/// Implementations own buffer export, codecs, serialization, queues, and
/// binding feedback. The relay owns surface admission and authorization.
pub trait HoistSourcePort {
    fn submit(&mut self, command: SourcePortCommand) -> HoistPortResult<()>;
    fn poll(&mut self) -> HoistPortResult<Vec<DestinationEnvelope>>;
    fn accept_destination(&mut self, envelope: &DestinationEnvelope) -> HoistPortResult<()>;
    fn effects_drained(&mut self);
    fn disconnect(&mut self);
}

pub struct DestinationPortRecord {
    pub session: HoistSessionId,
    pub event: DestinationPortEvent,
}

pub enum DestinationPortEvent {
    MappedSurface(ClientSurfaceId),
    Surface(ClientSurfaceEvent),
    WithdrawSurface(ClientSurfaceId),
    Ended,
    Cursor {
        update: weld_client::ClientCursorUpdate,
        sequence: u64,
    },
}

pub enum DestinationPortCommand {
    Message(DestinationEnvelope),
    RouteMapped {
        source: ClientSurfaceId,
        destination: ClientSurfaceId,
    },
    RouteUnmapped {
        destination: ClientSurfaceId,
    },
}

/// Destination-side binding boundary.
///
/// Implementations own buffer import, decoding, serialization, queues, and
/// optional same-runtime back-routing. The relay owns destination identity and
/// session authorization.
pub trait HoistDestinationPort {
    fn poll(&mut self) -> HoistPortResult<Vec<DestinationPortRecord>>;
    fn submit(&mut self, command: DestinationPortCommand) -> HoistPortResult<()>;
    fn drain_route_alias_updates(&mut self, _updates: &mut Vec<ClientRouteAliasUpdate>) {}
    fn disconnect(&mut self);
}

#[derive(Default)]
struct CachedSurface {
    role: Option<weld_client::ClientSurfaceRole>,
    commit: Option<ClientSurfaceCommit>,
    cursor: Option<weld_client::ClientCursor>,
    sent_cursor: Option<weld_client::ClientCursor>,
}

struct CursorInFlight {
    session: HoistSessionId,
    surface: ClientSurfaceId,
    sequence: u64,
    sent_at: Instant,
    warned: bool,
}

/// One source-side relay shared by loopback and external bindings.
pub struct SourceRelayAdapter {
    upstream_source: ClientSourceId,
    cache: HashMap<ClientSurfaceId, CachedSurface>,
    mappings: HashMap<ClientSurfaceId, HoistSessionId>,
    effects: Vec<ClientAdapterEffect>,
    failed: bool,
    remote_input: RemoteInputState,
    port: Box<dyn HoistSourcePort>,
    pending_cursors: VecDeque<ClientSurfaceId>,
    cursor_in_flight: Option<CursorInFlight>,
    next_cursor_sequence: Option<u64>,
}

impl SourceRelayAdapter {
    pub fn new(upstream_source: ClientSourceId, port: impl HoistSourcePort + 'static) -> Self {
        Self {
            upstream_source,
            cache: HashMap::new(),
            mappings: HashMap::new(),
            effects: Vec::new(),
            failed: false,
            remote_input: RemoteInputState::default(),
            port: Box::new(port),
            pending_cursors: VecDeque::new(),
            cursor_in_flight: None,
            next_cursor_sequence: Some(1),
        }
    }

    fn map(&mut self, session: HoistSessionId, source: ClientSurfaceId) {
        if source.source() != self.upstream_source || self.mappings.contains_key(&source) {
            return;
        }
        self.mappings.insert(source, session);
        if let Err(error) = self.port.submit(SourcePortCommand::MapSurface {
            session,
            surface: source,
        }) {
            self.fail(error);
            return;
        }
        if let Some((role, commit)) = self
            .cache
            .get(&source)
            .map(|cached| (cached.role, cached.commit.clone()))
        {
            if let Some(role) = role {
                self.send_surface(
                    session,
                    ClientSurfaceEvent {
                        surface: source,
                        kind: ClientSurfaceEventKind::Role(role),
                    },
                );
            }
            if let Some(commit) = commit {
                self.send_surface(
                    session,
                    ClientSurfaceEvent {
                        surface: source,
                        kind: ClientSurfaceEventKind::Commit(commit),
                    },
                );
            }
        }
        self.queue_cursor(source);
        let popups = self
            .cache
            .iter()
            .filter_map(|(surface, cached)| match cached.role {
                Some(weld_client::ClientSurfaceRole::Popup(popup)) if popup.owner == source => {
                    Some(*surface)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for popup in popups {
            self.map(session, popup);
        }
    }

    fn unmap(&mut self, source: ClientSurfaceId) {
        let Some(session) = self.mappings.remove(&source) else {
            return;
        };
        self.pending_cursors.retain(|surface| *surface != source);
        if let Some(cached) = self.cache.get_mut(&source) {
            cached.sent_cursor = None;
        }
        self.effects.extend(
            self.remote_input
                .release_effects(self.upstream_source, |surface| surface == source),
        );
        self.effects
            .push(ClientAdapterEffect::Request(ClientRequest::Surface(
                weld_client::ClientSurfaceRequest {
                    surface: source,
                    kind: ClientSurfaceRequestKind::SetPreferredScale { scale_120: None },
                },
            )));
        if let Err(error) = self.port.submit(SourcePortCommand::WithdrawSurface {
            session,
            surface: source,
        }) {
            self.fail(error);
            return;
        }
        let popups = self
            .mappings
            .keys()
            .filter(|surface| {
                matches!(
                    self.cache.get(surface).and_then(|cached| cached.role),
                    Some(weld_client::ClientSurfaceRole::Popup(popup)) if popup.owner == source
                )
            })
            .copied()
            .collect::<Vec<_>>();
        for popup in popups {
            self.unmap(popup);
        }
    }

    fn observe(&mut self, event: &ClientSurfaceEvent) {
        if event.surface.source() != self.upstream_source {
            return;
        }
        let source = event.surface;
        match &event.kind {
            ClientSurfaceEventKind::Role(role) => {
                self.cache.entry(source).or_default().role = Some(*role);
                // Already admitted popups still publish position and stack changes.
                // Only the initial map replays the cached role itself.
                if let Some(session) = self.mappings.get(&source).copied() {
                    self.send_surface(session, event.clone());
                } else if let weld_client::ClientSurfaceRole::Popup(popup) = role
                    && let Some(session) = self.mappings.get(&popup.owner).copied()
                {
                    self.map(session, source);
                }
            }
            ClientSurfaceEventKind::Commit(commit) => {
                let outgoing = commit.clone();
                let cached = self.cache.entry(source).or_default();
                if !commit.mapped {
                    cached.cursor = None;
                    cached.sent_cursor = None;
                    self.pending_cursors.retain(|surface| *surface != source);
                }
                let mut retained = commit.clone();
                if let Some(previous) = &mut cached.commit {
                    retained.carry_unobserved_content_from(previous);
                }
                cached.commit = Some(retained);
                if let Some(session) = self.mappings.get(&source).copied() {
                    self.send_surface(
                        session,
                        ClientSurfaceEvent {
                            surface: source,
                            kind: ClientSurfaceEventKind::Commit(outgoing),
                        },
                    );
                }
            }
            ClientSurfaceEventKind::Interaction(_) => {
                if let Some(session) = self.mappings.get(&source).copied() {
                    self.send_surface(session, event.clone());
                }
            }
            ClientSurfaceEventKind::Destroyed => {
                self.effects.extend(
                    self.remote_input
                        .release_effects(self.upstream_source, |surface| surface == source),
                );
                if let Some(session) = self.mappings.remove(&source) {
                    self.send_surface(session, event.clone());
                }
                self.cache.remove(&source);
                self.pending_cursors.retain(|surface| *surface != source);
            }
        }
    }

    fn send_surface(&mut self, session: HoistSessionId, event: ClientSurfaceEvent) {
        if self.failed {
            return;
        }
        if let Err(error) = self
            .port
            .submit(SourcePortCommand::Surface { session, event })
        {
            self.fail(error);
        }
    }

    fn poll(&mut self) {
        if self.failed {
            return;
        }
        if let Some(active) = &mut self.cursor_in_flight
            && !active.warned
            && active.sent_at.elapsed() >= Duration::from_secs(2)
        {
            active.warned = true;
            tracing::warn!(surface = ?active.surface, session = ?active.session,
                sequence = active.sequence, "cursor feedback is waiting for peer acknowledgement");
        }
        let envelopes = match self.port.poll() {
            Ok(envelopes) => envelopes,
            Err(error) => {
                self.fail(error);
                return;
            }
        };
        for envelope in envelopes {
            if !self.accept_destination(envelope) {
                break;
            }
        }
    }

    fn accept_destination(&mut self, envelope: DestinationEnvelope) -> bool {
        if let DestinationMessage::CursorReceived { surface, sequence } = envelope.message {
            let Some(active) = &self.cursor_in_flight else {
                if self
                    .next_cursor_sequence
                    .is_some_and(|next| sequence >= next)
                {
                    self.fail("cursor acknowledgement precedes transmission");
                    return false;
                }
                return true;
            };
            if sequence < active.sequence {
                return true;
            }
            if (envelope.session, surface, sequence)
                != (active.session, active.surface, active.sequence)
            {
                self.fail("cursor acknowledgement does not match the outstanding update");
                return false;
            }
            self.cursor_in_flight = None;
            self.send_next_cursor();
            return !self.failed;
        }
        let target =
            if let DestinationMessage::Request(ClientRequest::Focus(focus)) = &envelope.message {
                if focus.source != self.upstream_source {
                    // A wrong namespace is a protocol violation, not a stale route.
                    self.fail("destination focus targeted another source");
                    return false;
                }
                let target = focus.surface.or(self.remote_input.keyboard_focus);
                if target.is_none() {
                    return true;
                }
                target
            } else {
                destination_message_surface(&envelope.message)
            };
        if let Some(surface) = target {
            match self.mappings.get(&surface).copied() {
                Some(session) if session == envelope.session => {}
                Some(_) => {
                    self.fail("destination message crossed hoist sessions");
                    return false;
                }
                None => {
                    tracing::debug!(?surface, session = ?envelope.session,
                        message_kind = envelope.message.kind(),
                        "ignored destination message for an unmapped surface");
                    return true;
                }
            }
        }
        if let Err(error) = self.port.accept_destination(&envelope) {
            self.fail(error);
            return false;
        }
        match envelope.message {
            DestinationMessage::Request(request) => {
                self.remote_input.observe_request(&request);
                self.effects.push(ClientAdapterEffect::Request(request));
            }
            DestinationMessage::Input(input) => {
                let input = input.into_client_event();
                self.remote_input.observe_input(&input);
                self.effects.push(ClientAdapterEffect::Input(input));
            }
            DestinationMessage::BufferReleased { .. } | DestinationMessage::Reclaim => {}
            DestinationMessage::CursorReceived { .. } => {}
        }
        true
    }

    fn fail(&mut self, reason: impl Display) {
        if self.failed {
            return;
        }
        self.failed = true;
        self.cursor_in_flight = None;
        self.pending_cursors.clear();
        tracing::warn!(source = ?self.upstream_source, error = %reason, "hoist source relay failed");
        self.port.disconnect();
        self.effects.extend(
            self.remote_input
                .release_effects(self.upstream_source, |_| true),
        );
        self.effects
            .extend(self.mappings.keys().copied().map(|surface| {
                ClientAdapterEffect::Request(ClientRequest::Surface(
                    weld_client::ClientSurfaceRequest {
                        surface,
                        kind: ClientSurfaceRequestKind::SetPreferredScale { scale_120: None },
                    },
                ))
            }));
    }
}

impl SourceRelayAdapter {
    fn queue_cursor(&mut self, surface: ClientSurfaceId) {
        if !self.mappings.contains_key(&surface) || self.failed {
            return;
        }
        if !self.pending_cursors.contains(&surface) {
            self.pending_cursors.push_back(surface);
        }
        self.send_next_cursor();
    }

    fn send_next_cursor(&mut self) {
        if self.failed || self.cursor_in_flight.is_some() {
            return;
        }
        while let Some(surface) = self.pending_cursors.pop_front() {
            let Some(session) = self.mappings.get(&surface).copied() else {
                continue;
            };
            let Some(cached) = self.cache.get_mut(&surface) else {
                continue;
            };
            let Some(cursor) = &cached.cursor else {
                continue;
            };
            if cached.sent_cursor.as_ref() == Some(cursor) {
                continue;
            }
            let Some(sequence) = self.next_cursor_sequence else {
                self.fail("cursor sequence space is exhausted");
                return;
            };
            self.next_cursor_sequence = sequence.checked_add(1);
            cached.sent_cursor = Some(cursor.clone());
            let update = weld_client::ClientCursorUpdate {
                surface,
                cursor: cursor.clone(),
            };
            self.cursor_in_flight = Some(CursorInFlight {
                session,
                surface,
                sequence,
                sent_at: Instant::now(),
                warned: false,
            });
            if let Err(error) = self.port.submit(SourcePortCommand::Cursor {
                session,
                update,
                sequence,
            }) {
                self.fail(error);
            }
            return;
        }
    }
}

impl ClientAdapter for SourceRelayAdapter {
    fn observe_cursor_update(&mut self, update: &weld_client::ClientCursorUpdate) {
        if update.surface.source() != self.upstream_source || self.failed {
            return;
        }
        let cached = self.cache.entry(update.surface).or_default();
        if cached.cursor.as_ref() == Some(&update.cursor) {
            return;
        }
        cached.cursor = Some(update.cursor.clone());
        self.queue_cursor(update.surface);
    }
    fn drain_events(&mut self, _events: &mut ClientEventQueue) {
        self.poll();
    }

    fn apply_request(&mut self, _request: ClientRequest) {}
    fn apply_input(&mut self, _event: ClientInputEvent) {}

    fn apply_command(&mut self, command: ClientAdapterCommandEnvelope) {
        let Ok(command) = command.downcast::<HoistEndpointCommand>() else {
            return;
        };
        match *command {
            HoistEndpointCommand::Map { session, source } => self.map(session, source),
            HoistEndpointCommand::Unmap { source } => self.unmap(source),
        }
    }

    fn host_focus_lost(&mut self, _time: u32) {}

    fn observe_event(&mut self, event: &ClientSurfaceEvent) {
        self.observe(event);
    }

    fn drain_effects(&mut self, effects: &mut Vec<ClientAdapterEffect>) {
        effects.append(&mut self.effects);
        self.port.effects_drained();
    }

    fn observe_retired_buffer(&mut self, buffer: ClientBufferId) {
        if buffer.source() == self.upstream_source
            && let Err(error) = self
                .port
                .submit(SourcePortCommand::RetireUpstreamBuffer(buffer))
        {
            self.fail(error);
        }
    }
}

/// One destination-side relay shared by loopback and external bindings.
pub struct DestinationRelayAdapter {
    upstream_source: ClientSourceId,
    descriptor: ClientSourceDescriptor,
    sessions: HashMap<ClientSurfaceId, HoistSessionId>,
    roles: HashMap<ClientSurfaceId, weld_client::ClientSurfaceRole>,
    events: ClientEventQueue,
    input: RemoteInputState,
    failed: bool,
    port: Box<dyn HoistDestinationPort>,
    cursor_updates: HashMap<ClientSurfaceId, weld_client::ClientCursor>,
}

impl DestinationRelayAdapter {
    pub fn new(
        upstream_source: ClientSourceId,
        descriptor: ClientSourceDescriptor,
        port: impl HoistDestinationPort + 'static,
    ) -> Self {
        Self {
            upstream_source,
            descriptor,
            sessions: HashMap::new(),
            roles: HashMap::new(),
            events: ClientEventQueue::default(),
            input: RemoteInputState::default(),
            failed: false,
            port: Box::new(port),
            cursor_updates: HashMap::new(),
        }
    }

    fn poll(&mut self) {
        if self.failed {
            return;
        }
        let records = match self.port.poll() {
            Ok(records) => records,
            Err(error) => {
                self.fail(error);
                return;
            }
        };
        for record in records {
            if !self.apply_record(record) {
                break;
            }
        }
    }

    fn apply_record(&mut self, record: DestinationPortRecord) -> bool {
        match record.event {
            DestinationPortEvent::Cursor { update, sequence } => {
                if update.surface.source() != self.upstream_source {
                    self.fail("cursor feedback targeted another source");
                    return false;
                }
                if let Some(session) = self.sessions.get(&update.surface) {
                    if *session != record.session {
                        self.fail("cursor feedback crossed hoist sessions");
                        return false;
                    }
                    self.cursor_updates.insert(
                        relocated_surface(self.descriptor.id, update.surface),
                        update.cursor,
                    );
                }
                // Even withdrawn feedback must release the source's global slot.
                self.send_destination(
                    record.session,
                    DestinationMessage::CursorReceived {
                        surface: update.surface,
                        sequence,
                    },
                );
            }
            DestinationPortEvent::MappedSurface(source) => {
                if !self.map_surface(record.session, source) {
                    return false;
                }
            }
            DestinationPortEvent::Surface(event) => {
                let source = event.surface;
                if source.source() != self.upstream_source {
                    self.fail("source event targeted another source");
                    return false;
                }
                if self.sessions.get(&source).copied() != Some(record.session) {
                    self.fail(
                        "source event targeted an unmapped surface or crossed hoist sessions",
                    );
                    return false;
                }
                if matches!(event.kind, ClientSurfaceEventKind::Destroyed) {
                    self.destroy_surface(source, true);
                    return true;
                }
                // Unlike the source's conservative reset on unmap, the
                // receiver retains cursor preference: newer cursor control can
                // overtake this video-delayed commit. Runtime mapping gates
                // visibility; destruction/withdrawal still discard feedback.
                match event.kind {
                    ClientSurfaceEventKind::Role(role) => {
                        self.roles.insert(source, role);
                        if let Some(role) = self.relocated_role(role) {
                            self.events.push(ClientSurfaceEvent {
                                surface: relocated_surface(self.descriptor.id, source),
                                kind: ClientSurfaceEventKind::Role(role),
                            });
                        }
                    }
                    kind => self.events.push(ClientSurfaceEvent {
                        surface: relocated_surface(self.descriptor.id, source),
                        kind,
                    }),
                }
            }
            DestinationPortEvent::WithdrawSurface(surface) => self.destroy_surface(surface, true),
            DestinationPortEvent::Ended => self.end_session(record.session),
        }
        true
    }

    fn map_surface(&mut self, session: HoistSessionId, source: ClientSurfaceId) -> bool {
        if source.source() != self.upstream_source {
            self.fail("surface mapping targeted another source");
            return false;
        }
        match self.sessions.get(&source).copied() {
            Some(current) if current == session => return true,
            Some(_) => {
                self.fail("surface mapping crossed hoist sessions");
                return false;
            }
            None => {}
        }
        self.sessions.insert(source, session);
        let destination = relocated_surface(self.descriptor.id, source);
        if let Err(error) = self.port.submit(DestinationPortCommand::RouteMapped {
            source,
            destination,
        }) {
            self.fail(error);
            return false;
        }
        self.refresh_children_of(source);
        true
    }

    fn relocated_role(
        &self,
        role: weld_client::ClientSurfaceRole,
    ) -> Option<weld_client::ClientSurfaceRole> {
        match role {
            weld_client::ClientSurfaceRole::Toplevel(toplevel) => Some(
                weld_client::ClientSurfaceRole::Toplevel(weld_client::ToplevelState {
                    parent: toplevel.parent.and_then(|parent| {
                        self.sessions
                            .contains_key(&parent)
                            .then(|| relocated_surface(self.descriptor.id, parent))
                    }),
                    decoration: toplevel.decoration,
                }),
            ),
            weld_client::ClientSurfaceRole::Popup(popup) => {
                let owner = self
                    .sessions
                    .contains_key(&popup.owner)
                    .then(|| relocated_surface(self.descriptor.id, popup.owner))?;
                Some(weld_client::ClientSurfaceRole::Popup(
                    weld_client::PopupState { owner, ..popup },
                ))
            }
        }
    }

    fn refresh_children_of(&mut self, parent: ClientSurfaceId) {
        let children = self
            .roles
            .iter()
            .filter_map(|(surface, role)| match role {
                weld_client::ClientSurfaceRole::Toplevel(weld_client::ToplevelState {
                    parent: Some(candidate),
                    ..
                }) if *candidate == parent && self.sessions.contains_key(surface) => {
                    Some((*surface, *role))
                }
                weld_client::ClientSurfaceRole::Popup(popup)
                    if popup.owner == parent && self.sessions.contains_key(surface) =>
                {
                    Some((*surface, *role))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for (source, role) in children {
            if let Some(role) = self.relocated_role(role) {
                self.events.push(ClientSurfaceEvent {
                    surface: relocated_surface(self.descriptor.id, source),
                    kind: ClientSurfaceEventKind::Role(role),
                });
            }
        }
    }

    fn destroy_surface(&mut self, source: ClientSurfaceId, notify_port: bool) {
        self.cursor_updates
            .remove(&relocated_surface(self.descriptor.id, source));
        if self.sessions.remove(&source).is_none() {
            return;
        }
        self.roles.remove(&source);
        let destination = relocated_surface(self.descriptor.id, source);
        // The source settles withdrawn input before removing its mapping;
        // transport failure instead makes its disconnect path settle all input.
        // Forget locally without emitting late packets for the retired route.
        drop(
            self.input
                .release_effects(self.descriptor.id, |surface| surface == destination),
        );
        if notify_port
            && let Err(error) = self
                .port
                .submit(DestinationPortCommand::RouteUnmapped { destination })
        {
            self.fail(error);
        }
        self.events.push(ClientSurfaceEvent {
            surface: destination,
            kind: ClientSurfaceEventKind::Destroyed,
        });
    }

    fn end_session(&mut self, session: HoistSessionId) {
        let surfaces = self
            .sessions
            .iter()
            .filter_map(|(surface, candidate)| (*candidate == session).then_some(*surface))
            .collect::<Vec<_>>();
        for surface in surfaces {
            self.destroy_surface(surface, true);
        }
    }

    fn fail(&mut self, reason: impl Display) {
        if self.failed {
            return;
        }
        self.failed = true;
        tracing::warn!(source = ?self.upstream_source, destination = ?self.descriptor.id,
            error = %reason, "hoist destination relay failed");
        self.port.disconnect();
        let surfaces = self.sessions.keys().copied().collect::<Vec<_>>();
        for surface in surfaces {
            self.destroy_surface(surface, false);
        }
    }

    fn source_surface(&self, destination: ClientSurfaceId) -> Option<ClientSurfaceId> {
        (destination.source() == self.descriptor.id).then(|| {
            ClientSurfaceId::new(
                weld_client::ClientId::new(self.upstream_source, destination.client().local()),
                destination.local(),
            )
        })
    }

    fn send_destination(&mut self, session: HoistSessionId, message: DestinationMessage) {
        if let Err(error) = self
            .port
            .submit(DestinationPortCommand::Message(DestinationEnvelope {
                session,
                message,
            }))
        {
            self.fail(error);
        }
    }
}

impl ClientAdapter for DestinationRelayAdapter {
    fn drain_cursor_updates(&mut self, updates: &mut Vec<weld_client::ClientCursorUpdate>) {
        updates.extend(
            self.cursor_updates
                .drain()
                .map(|(surface, cursor)| weld_client::ClientCursorUpdate { surface, cursor }),
        );
    }
    fn drain_events(&mut self, events: &mut ClientEventQueue) {
        self.poll();
        while let Some(event) = self.events.pop_front() {
            events.push(event);
        }
    }

    fn apply_request(&mut self, mut request: ClientRequest) {
        let destination = match &request {
            ClientRequest::Focus(focus) if focus.surface.is_none() => self.input.keyboard_focus,
            _ => request_surface(&request),
        };
        let Some(destination) = destination else {
            return;
        };
        let Some(source) = self.source_surface(destination) else {
            return;
        };
        let Some(session) = self.sessions.get(&source).copied() else {
            return;
        };
        self.input.observe_request(&request);
        match &mut request {
            ClientRequest::Focus(focus) if focus.surface.is_none() => {
                focus.source = self.upstream_source;
            }
            ClientRequest::Focus(_) => {
                rewrite_request_surface(&mut request, source);
            }
            ClientRequest::Surface(_) | ClientRequest::ClearFocus => {
                rewrite_request_surface(&mut request, source);
            }
        }
        self.send_destination(session, DestinationMessage::Request(request));
    }

    fn apply_input(&mut self, mut event: ClientInputEvent) {
        let destination = event.target.surface();
        let Some(source) = self.source_surface(destination) else {
            return;
        };
        let Some(session) = self.sessions.get(&source).copied() else {
            return;
        };
        self.input.observe_input(&event);
        event.target = match event.target {
            ClientInputTarget::Pointer { layer, .. } => ClientInputTarget::Pointer {
                surface: source,
                layer,
            },
            ClientInputTarget::Keyboard { .. } => ClientInputTarget::Keyboard { surface: source },
        };
        self.send_destination(session, DestinationMessage::input(event));
    }

    fn apply_command(&mut self, _command: ClientAdapterCommandEnvelope) {}
    fn host_focus_lost(&mut self, time: u32) {
        self.input.last_time = time;
        let previous_focus = self.input.keyboard_focus;
        let effects = self.input.release_effects(self.descriptor.id, |_| true);
        for effect in effects {
            if self.failed {
                break;
            }
            match effect {
                ClientAdapterEffect::Input(input) => self.apply_input(input),
                ClientAdapterEffect::Request(request) => {
                    // apply_request needs the previous destination to route a
                    // source-qualified focus clear after the ledger is drained.
                    self.input.keyboard_focus = previous_focus;
                    self.apply_request(request);
                }
            }
        }
    }

    fn drain_route_alias_updates(&mut self, updates: &mut Vec<ClientRouteAliasUpdate>) {
        self.port.drain_route_alias_updates(updates);
    }
}

fn destination_message_surface(message: &DestinationMessage) -> Option<ClientSurfaceId> {
    match message {
        DestinationMessage::Request(request) => request_surface(request),
        DestinationMessage::Input(input) => Some(input.target.surface()),
        DestinationMessage::BufferReleased { .. }
        | DestinationMessage::Reclaim
        | DestinationMessage::CursorReceived { .. } => None,
    }
}

fn request_surface(request: &ClientRequest) -> Option<ClientSurfaceId> {
    match request {
        ClientRequest::Surface(request) => Some(request.surface),
        ClientRequest::Focus(request) => request.surface,
        ClientRequest::ClearFocus => None,
    }
}

fn rewrite_request_surface(request: &mut ClientRequest, source: ClientSurfaceId) {
    match request {
        ClientRequest::Surface(request) => {
            request.surface = source;
            if let ClientSurfaceRequestKind::SetOutputs {
                preferred_scale_120,
                ..
            } = request.kind
            {
                request.kind = ClientSurfaceRequestKind::SetPreferredScale {
                    scale_120: preferred_scale_120,
                };
            }
        }
        ClientRequest::Focus(request) => {
            request.source = source.source();
            request.surface = Some(source);
        }
        ClientRequest::ClearFocus => {}
    }
}

#[derive(Default)]
struct RemoteInputState {
    keys: HashMap<LinuxKeycode, ClientInputTarget>,
    buttons: HashMap<LinuxButtonCode, (ClientInputTarget, Option<InputPosition>)>,
    gestures: Vec<(PointerGestureKind, ClientInputTarget)>,
    finger_scroll: Option<RemoteFingerScroll>,
    keyboard_focus: Option<ClientSurfaceId>,
    last_time: u32,
}

struct RemoteFingerScroll {
    target: ClientInputTarget,
    horizontal_active: bool,
    vertical_active: bool,
}

impl RemoteInputState {
    fn observe_request(&mut self, request: &ClientRequest) {
        if let ClientRequest::Focus(focus) = request {
            self.keyboard_focus = focus.surface;
        }
    }

    fn observe_input(&mut self, input: &ClientInputEvent) {
        self.last_time = input.time;
        match &input.event {
            InputEventKind::PointerButton {
                position,
                button,
                state,
            } => match state {
                weld_client::ButtonState::Pressed => {
                    self.buttons.insert(*button, (input.target, *position));
                }
                weld_client::ButtonState::Released => {
                    self.buttons.remove(button);
                }
            },
            InputEventKind::PointerAxis { axis, .. } if axis.source == RawScrollSource::Finger => {
                match axis.phase {
                    RawScrollPhase::Started => {
                        self.finger_scroll = Some(RemoteFingerScroll {
                            target: input.target,
                            horizontal_active: axis.horizontal != 0.0,
                            vertical_active: axis.vertical != 0.0,
                        });
                    }
                    RawScrollPhase::Moved => {
                        if let Some(scroll) = &mut self.finger_scroll {
                            scroll.horizontal_active |= axis.horizontal != 0.0;
                            scroll.vertical_active |= axis.vertical != 0.0;
                            scroll.horizontal_active &= !axis.horizontal_stop;
                            scroll.vertical_active &= !axis.vertical_stop;
                        }
                    }
                    RawScrollPhase::Ended | RawScrollPhase::Cancelled => {
                        self.finger_scroll = None;
                    }
                }
            }
            InputEventKind::PointerGesture { gesture } => {
                if gesture.is_begin() {
                    self.gestures.push((gesture.kind(), input.target));
                } else if gesture.is_end() {
                    self.gestures.retain(|(kind, _)| *kind != gesture.kind());
                }
            }
            InputEventKind::Keyboard { keycode, state } => match state {
                weld_client::ButtonState::Pressed => {
                    self.keys.insert(*keycode, input.target);
                }
                weld_client::ButtonState::Released => {
                    self.keys.remove(keycode);
                }
            },
            InputEventKind::PointerMotion { position } => {
                for (target, last_position) in self.buttons.values_mut() {
                    if *target == input.target {
                        *last_position = Some(*position);
                    }
                }
            }
            InputEventKind::PointerLeft { .. } | InputEventKind::PointerAxis { .. } => {}
        }
    }

    fn release_effects(
        &mut self,
        source: ClientSourceId,
        should_release: impl Fn(ClientSurfaceId) -> bool,
    ) -> Vec<ClientAdapterEffect> {
        let mut effects = Vec::new();
        let time = self.last_time;
        effects.extend(
            self.buttons
                .extract_if(|_, (target, _)| should_release(target.surface()))
                .map(|(button, (target, position))| {
                    ClientAdapterEffect::Input(ClientInputEvent {
                        target,
                        host_position: None,
                        event: InputEventKind::PointerButton {
                            position,
                            button,
                            state: weld_client::ButtonState::Released,
                        },
                        time,
                    })
                }),
        );
        effects.extend(
            self.keys
                .extract_if(|_, target| should_release(target.surface()))
                .map(|(keycode, target)| {
                    ClientAdapterEffect::Input(ClientInputEvent {
                        target,
                        host_position: None,
                        event: InputEventKind::Keyboard {
                            keycode,
                            state: weld_client::ButtonState::Released,
                        },
                        time,
                    })
                }),
        );
        effects.extend(
            self.gestures
                .extract_if(.., |(_, target)| should_release(target.surface()))
                .map(|(kind, target)| {
                    ClientAdapterEffect::Input(ClientInputEvent {
                        target,
                        host_position: None,
                        event: InputEventKind::PointerGesture {
                            gesture: kind.cancelled(),
                        },
                        time,
                    })
                }),
        );
        if self
            .finger_scroll
            .as_ref()
            .is_some_and(|scroll| should_release(scroll.target.surface()))
            && let Some(scroll) = self.finger_scroll.take()
        {
            effects.push(ClientAdapterEffect::Input(ClientInputEvent {
                target: scroll.target,
                host_position: None,
                event: InputEventKind::PointerAxis {
                    position: None,
                    axis: weld_client::RawScrollFrame::cancelled_finger(
                        scroll.horizontal_active,
                        scroll.vertical_active,
                    ),
                },
                time,
            }));
        }
        if self.keyboard_focus.is_some_and(should_release) {
            self.keyboard_focus = None;
            effects.push(ClientAdapterEffect::Request(ClientRequest::Focus(
                weld_client::ClientFocusRequest {
                    source,
                    surface: None,
                },
            )));
        }
        effects
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, fmt, rc::Rc};

    use weld_client::{
        ButtonState, ClientAdapter, ClientAdapterCommandEnvelope, ClientFocusRequest, ClientId,
        ClientInputEvent, ClientInputTarget, ClientKeyboardRoute, ClientProvenance, ClientRequest,
        ClientRuntime, ClientRuntimeAdapter, ClientSurfaceRequest, ClientSurfaceRequestKind,
        ClientSurfaceRole, InputEventKind, LinuxKeycode, LogicalPoint, PopupState,
        RuntimeInputEvent, RuntimeInputEventKind,
    };

    use super::*;

    #[derive(Default)]
    struct FakeSourceState {
        inbound: Vec<DestinationEnvelope>,
        submitted: Vec<SourcePortCommand>,
        accepted: usize,
        fail_poll: bool,
        fail_withdraw: bool,
        disconnected: bool,
    }

    struct FakeSourcePort(Rc<RefCell<FakeSourceState>>);

    #[derive(Default)]
    struct FakeDestinationState {
        inbound: Vec<DestinationPortRecord>,
        outbound: Vec<DestinationPortCommand>,
    }

    #[derive(Default)]
    struct FakeDestinationPort(Rc<RefCell<FakeDestinationState>>);

    impl HoistDestinationPort for FakeDestinationPort {
        fn poll(&mut self) -> HoistPortResult<Vec<DestinationPortRecord>> {
            Ok(std::mem::take(&mut self.0.borrow_mut().inbound))
        }

        fn submit(&mut self, command: DestinationPortCommand) -> HoistPortResult<()> {
            self.0.borrow_mut().outbound.push(command);
            Ok(())
        }

        fn disconnect(&mut self) {}
    }

    #[test]
    fn popup_role_waits_for_owner_mapping_and_keeps_the_latest_position() {
        let source = ClientSourceId::new(1);
        let destination = ClientSourceId::new(2);
        let owner = surface(source, 1);
        let popup = surface(source, 2);
        let session = HoistSessionId::new(1);
        let mut relay = DestinationRelayAdapter::new(
            source,
            ClientSourceDescriptor::new(destination, ClientProvenance::Relocated),
            FakeDestinationPort::default(),
        );
        assert!(relay.apply_record(DestinationPortRecord {
            session,
            event: DestinationPortEvent::MappedSurface(popup),
        }));
        for position in [LogicalPoint::ZERO, LogicalPoint::new(450.0, -20.0)] {
            assert!(relay.apply_record(DestinationPortRecord {
                session,
                event: DestinationPortEvent::Surface(ClientSurfaceEvent {
                    surface: popup,
                    kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(PopupState {
                        owner,
                        position,
                        stack_index: 3,
                    })),
                }),
            }));
        }
        assert!(relay.events.is_empty());
        assert!(relay.apply_record(DestinationPortRecord {
            session,
            event: DestinationPortEvent::MappedSurface(owner),
        }));
        let event = relay.events.pop_front().expect("delayed popup role");
        assert_eq!(event.surface, relocated_surface(destination, popup));
        assert!(matches!(event.kind,
            ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(role))
                if role == PopupState {
                    owner: relocated_surface(destination, owner),
                    position: LogicalPoint::new(450.0, -20.0),
                    stack_index: 3,
                }
        ));
        assert!(relay.events.is_empty());
    }

    #[derive(Debug)]
    struct FakePortFailure;

    impl fmt::Display for FakePortFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake port failure")
        }
    }

    impl Error for FakePortFailure {}

    impl HoistSourcePort for FakeSourcePort {
        fn submit(&mut self, command: SourcePortCommand) -> HoistPortResult<()> {
            if self.0.borrow().fail_withdraw
                && matches!(&command, SourcePortCommand::WithdrawSurface { .. })
            {
                return Err(Box::new(FakePortFailure));
            }
            self.0.borrow_mut().submitted.push(command);
            Ok(())
        }

        fn poll(&mut self) -> HoistPortResult<Vec<DestinationEnvelope>> {
            let mut state = self.0.borrow_mut();
            if state.fail_poll {
                return Err(Box::new(FakePortFailure));
            }
            Ok(std::mem::take(&mut state.inbound))
        }

        fn accept_destination(&mut self, _envelope: &DestinationEnvelope) -> HoistPortResult<()> {
            self.0.borrow_mut().accepted += 1;
            Ok(())
        }

        fn effects_drained(&mut self) {}

        fn disconnect(&mut self) {
            self.0.borrow_mut().disconnected = true;
        }
    }

    fn surface(source: ClientSourceId, local: u64) -> ClientSurfaceId {
        ClientSurfaceId::new(ClientId::new(source, 1), local)
    }

    fn mapped_source() -> (
        SourceRelayAdapter,
        Rc<RefCell<FakeSourceState>>,
        ClientSurfaceId,
        HoistSessionId,
    ) {
        let source = ClientSourceId::new(4);
        let surface = surface(source, 7);
        let session = HoistSessionId::new(9);
        let state = Rc::new(RefCell::new(FakeSourceState::default()));
        let mut adapter = SourceRelayAdapter::new(source, FakeSourcePort(state.clone()));
        adapter.apply_command(ClientAdapterCommandEnvelope::new(
            ClientSourceId::new(8),
            HoistEndpointCommand::Map {
                session,
                source: surface,
            },
        ));
        (adapter, state, surface, session)
    }

    #[test]
    fn cursor_credit_survives_withdrawal_and_bounds_updates_without_blocking_input() {
        let (mut relay, port, surface, session) = mapped_source();
        relay.observe_cursor_update(&weld_client::ClientCursorUpdate {
            surface,
            cursor: weld_client::ClientCursor::Hidden,
        });
        let sequence = relay
            .cursor_in_flight
            .as_ref()
            .expect("outstanding cursor")
            .sequence;
        for index in 0..300 {
            relay.observe_cursor_update(&weld_client::ClientCursorUpdate {
                surface,
                cursor: weld_client::ClientCursor::Named(if index % 2 == 0 {
                    weld_client::CursorIcon::Text
                } else {
                    weld_client::CursorIcon::Pointer
                }),
            });
        }
        assert_eq!(
            port.borrow()
                .submitted
                .iter()
                .filter(|command| matches!(command, SourcePortCommand::Cursor { .. }))
                .count(),
            1
        );
        assert_eq!(relay.pending_cursors.len(), 1);
        assert!(relay.accept_destination(DestinationEnvelope {
            session,
            message: DestinationMessage::input(key_input(surface, 30, ButtonState::Pressed))
        }));
        assert_eq!(
            port.borrow().accepted,
            1,
            "unrelated input is not cursor-credit gated"
        );
        relay.send_surface(
            session,
            ClientSurfaceEvent {
                surface,
                kind: ClientSurfaceEventKind::Interaction(
                    weld_client::ToplevelInteractionRequestKind::End,
                ),
            },
        );
        assert!(matches!(
            port.borrow().submitted.last(),
            Some(SourcePortCommand::Surface { .. })
        ));
        relay.unmap(surface);
        relay.map(session, surface);
        assert_eq!(
            relay
                .cursor_in_flight
                .as_ref()
                .expect("still waiting")
                .sequence,
            sequence
        );
        assert!(relay.accept_destination(DestinationEnvelope {
            session,
            message: DestinationMessage::CursorReceived { surface, sequence }
        }));
        let next = relay
            .cursor_in_flight
            .as_ref()
            .expect("latest replay")
            .sequence;
        assert!(next > sequence);
        assert!(relay.accept_destination(DestinationEnvelope {
            session,
            message: DestinationMessage::CursorReceived { surface, sequence }
        }));
        assert_eq!(
            relay
                .cursor_in_flight
                .as_ref()
                .expect("old ack cannot release new cursor")
                .sequence,
            next
        );
        relay.observe(&ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Destroyed,
        });
        assert!(relay.accept_destination(DestinationEnvelope {
            session,
            message: DestinationMessage::CursorReceived {
                surface,
                sequence: next
            }
        }));
        assert!(relay.cursor_in_flight.is_none());
        assert!(relay.pending_cursors.is_empty());
        assert!(!relay.failed);
        assert_eq!(
            destination_message_surface(&DestinationMessage::CursorReceived {
                surface,
                sequence: next
            }),
            None
        );
    }

    #[test]
    fn cursor_acknowledgements_validate_session_and_disconnect_clears_the_slot() {
        let (mut relay, _, surface, _) = mapped_source();
        relay.observe_cursor_update(&weld_client::ClientCursorUpdate {
            surface,
            cursor: weld_client::ClientCursor::Hidden,
        });
        let sequence = relay.cursor_in_flight.as_ref().expect("cursor").sequence;
        assert!(!relay.accept_destination(DestinationEnvelope {
            session: HoistSessionId::new(999),
            message: DestinationMessage::CursorReceived { surface, sequence }
        }));
        assert!(relay.failed);
        assert!(relay.cursor_in_flight.is_none());
    }

    #[test]
    fn destination_relocates_cursor_feedback_and_acks_withdrawn_updates_without_resurrection() {
        let source = ClientSourceId::new(1);
        let destination = ClientSourceId::new(2);
        let surface = surface(source, 7);
        let session = HoistSessionId::new(1);
        let port = Rc::new(RefCell::new(FakeDestinationState::default()));
        let mut relay = DestinationRelayAdapter::new(
            source,
            ClientSourceDescriptor::new(destination, ClientProvenance::Relocated),
            FakeDestinationPort(port.clone()),
        );
        relay.map_surface(session, surface);
        let feedback = |session, sequence| DestinationPortRecord {
            session,
            event: DestinationPortEvent::Cursor {
                update: weld_client::ClientCursorUpdate {
                    surface,
                    cursor: weld_client::ClientCursor::Named(weld_client::CursorIcon::Text),
                },
                sequence,
            },
        };
        assert!(relay.apply_record(feedback(session, 1)));
        let mut updates = Vec::new();
        relay.drain_cursor_updates(&mut updates);
        assert_eq!(updates[0].surface, relocated_surface(destination, surface));
        relay.destroy_surface(surface, true);
        assert!(relay.apply_record(feedback(session, 2)));
        assert!(relay.cursor_updates.is_empty());
        assert!(matches!(
            port.borrow().outbound.last(),
            Some(DestinationPortCommand::Message(DestinationEnvelope {
                message: DestinationMessage::CursorReceived { sequence: 2, .. },
                ..
            }))
        ));
        relay.map_surface(session, surface);
        assert!(!relay.apply_record(feedback(HoistSessionId::new(9), 3)));
        assert!(relay.failed);
    }

    fn key_input(surface: ClientSurfaceId, keycode: u32, state: ButtonState) -> ClientInputEvent {
        ClientInputEvent {
            target: ClientInputTarget::Keyboard { surface },
            host_position: None,
            event: InputEventKind::Keyboard {
                keycode: LinuxKeycode(keycode),
                state,
            },
            time: 15,
        }
    }

    fn sent_messages(state: &Rc<RefCell<FakeDestinationState>>) -> Vec<DestinationEnvelope> {
        std::mem::take(&mut state.borrow_mut().outbound)
            .into_iter()
            .filter_map(|command| match command {
                DestinationPortCommand::Message(envelope) => Some(envelope),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn destination_host_loss_releases_forwarded_input_once() {
        let upstream = ClientSourceId::new(4);
        let descriptor =
            ClientSourceDescriptor::new(ClientSourceId::new(9), ClientProvenance::Relocated);
        let original = surface(upstream, 7);
        let destination = relocated_surface(descriptor.id, original);
        let port = FakeDestinationPort::default();
        let state = port.0.clone();
        let mut relay = DestinationRelayAdapter::new(upstream, descriptor, port);
        assert!(relay.map_surface(HoistSessionId::new(1), original));
        relay.apply_request(ClientRequest::Focus(ClientFocusRequest {
            source: descriptor.id,
            surface: Some(destination),
        }));
        relay.apply_input(key_input(destination, 42, ButtonState::Pressed));
        let pointer = ClientInputTarget::Pointer {
            surface: destination,
            layer: weld_client::SurfaceLayerId::new(1),
        };
        let position = InputPosition::new(10.0, 20.0);
        for event in [
            InputEventKind::PointerButton {
                position: Some(position),
                button: LinuxButtonCode(0x110),
                state: ButtonState::Pressed,
            },
            InputEventKind::PointerGesture {
                gesture: weld_client::PointerGesture::Swipe(weld_client::TouchpadSwipe::Begin {
                    fingers: 3,
                }),
            },
            InputEventKind::PointerAxis {
                position: Some(position),
                axis: weld_client::RawScrollFrame {
                    source: RawScrollSource::Finger,
                    phase: RawScrollPhase::Started,
                    horizontal: 1.0,
                    vertical: 2.0,
                    horizontal_v120: None,
                    vertical_v120: None,
                    horizontal_stop: false,
                    vertical_stop: false,
                },
            },
            InputEventKind::PointerMotion {
                position: InputPosition::new(30.0, 40.0),
            },
        ] {
            relay.apply_input(ClientInputEvent {
                target: pointer,
                host_position: None,
                event,
                time: 20,
            });
        }
        sent_messages(&state);
        relay.host_focus_lost(30);
        let messages = sent_messages(&state);
        assert_eq!(
            messages.len(),
            5,
            "key, button, gesture, scroll and focus must settle"
        );
        assert!(messages.iter().any(|message| matches!(&message.message,
            DestinationMessage::Input(input) if input.time == 30 && input.event == key_input(original, 42, ButtonState::Released).event
        )));
        assert!(messages.iter().any(|message| matches!(&message.message,
            DestinationMessage::Input(input) if input.event == InputEventKind::PointerGesture { gesture: PointerGestureKind::Swipe.cancelled() }
        )));
        assert!(messages.iter().any(|message| matches!(&message.message,
            DestinationMessage::Input(input) if matches!(input.event, InputEventKind::PointerAxis { axis, .. }
                if axis.phase == RawScrollPhase::Cancelled && axis.horizontal_stop && axis.vertical_stop)
        )));
        assert!(messages.iter().any(|message| matches!(&message.message,
            DestinationMessage::Request(ClientRequest::Focus(focus)) if focus.source == upstream && focus.surface.is_none()
        )));
        assert!(messages.iter().any(|message| matches!(&message.message,
            DestinationMessage::Input(input) if matches!(input.event,
                InputEventKind::PointerButton { state: ButtonState::Released, position: Some(p), .. } if p == InputPosition::new(30.0, 40.0)
            )
        )));
        relay.host_focus_lost(31);
        assert!(sent_messages(&state).is_empty());
        assert!(!relay.failed);
    }

    #[test]
    fn reclaim_releases_only_the_selected_surface_input() {
        let (mut relay, _, first, session) = mapped_source();
        let second = surface(first.source(), 8);
        relay.map(HoistSessionId::new(10), second);
        for (surface, session, key) in [(first, session, 42), (second, HoistSessionId::new(10), 29)]
        {
            assert!(relay.accept_destination(DestinationEnvelope {
                session,
                message: DestinationMessage::input(key_input(surface, key, ButtonState::Pressed))
            }));
        }
        relay.effects.clear();
        relay.unmap(first);
        let releases = relay
            .effects
            .iter()
            .filter(|effect| matches!(effect, ClientAdapterEffect::Input(_)))
            .collect::<Vec<_>>();
        assert_eq!(
            releases,
            [&ClientAdapterEffect::Input(key_input(
                first,
                42,
                ButtonState::Released
            ))]
        );
        relay.effects.clear();
        relay.unmap(second);
        assert!(
            relay
                .effects
                .contains(&ClientAdapterEffect::Input(key_input(
                    second,
                    29,
                    ButtonState::Released
                )))
        );
    }

    #[test]
    fn reclaim_while_runtime_holds_a_key_releases_upstream_exactly_once() {
        let (mut source, _, original, session) = mapped_source();
        let descriptor =
            ClientSourceDescriptor::new(ClientSourceId::new(9), ClientProvenance::Relocated);
        let destination = relocated_surface(descriptor.id, original);
        let port = FakeDestinationPort::default();
        let state = port.0.clone();
        let mut relay = DestinationRelayAdapter::new(original.source(), descriptor, port);
        assert!(relay.map_surface(session, original));
        let mut runtime = ClientRuntime::default();
        runtime
            .register(ClientRuntimeAdapter::new(descriptor, relay))
            .expect("destination adapter");
        runtime.set_keyboard_route(Some(ClientKeyboardRoute {
            surface: destination,
        }));
        let event = |state| {
            RuntimeInputEvent::new(
                RuntimeInputEventKind::Input(key_input(destination, 42, state).event),
                15,
            )
        };
        runtime.dispatch_unconsumed_input(event(ButtonState::Pressed));
        for message in sent_messages(&state) {
            assert!(source.accept_destination(message));
        }
        source.effects.clear();
        source.unmap(original);
        state.borrow_mut().inbound.push(DestinationPortRecord {
            session,
            event: DestinationPortEvent::WithdrawSurface(original),
        });
        runtime.drain_events(&mut ClientEventQueue::default(), &mut Vec::new());
        runtime.dispatch_unconsumed_input(event(ButtonState::Released));
        runtime.host_focus_lost(20);
        for message in sent_messages(&state) {
            assert!(source.accept_destination(message));
        }
        let releases = source
            .effects
            .iter()
            .filter(|effect| {
                matches!(
                    effect,
                    ClientAdapterEffect::Input(ClientInputEvent {
                        event: InputEventKind::Keyboard {
                            state: ButtonState::Released,
                            ..
                        },
                        ..
                    })
                )
            })
            .count();
        assert_eq!(releases, 1);
    }

    #[test]
    fn remote_focus_clear_is_authorized_by_the_focused_session() {
        let (mut relay, _, surface, session) = mapped_source();
        assert!(relay.accept_destination(DestinationEnvelope {
            session,
            message: DestinationMessage::Request(ClientRequest::Focus(ClientFocusRequest {
                source: surface.source(),
                surface: Some(surface)
            }))
        }));
        relay.effects.clear();
        assert!(!relay.accept_destination(DestinationEnvelope {
            session: HoistSessionId::new(123),
            message: DestinationMessage::Request(ClientRequest::Focus(ClientFocusRequest {
                source: surface.source(),
                surface: None
            })),
        }));
    }

    #[test]
    fn transported_requests_and_input_are_forwarded_after_session_authorization() {
        let (mut adapter, state, surface, session) = mapped_source();
        let request = ClientRequest::Surface(ClientSurfaceRequest {
            surface,
            kind: ClientSurfaceRequestKind::Close,
        });
        let input = ClientInputEvent {
            target: ClientInputTarget::Keyboard { surface },
            host_position: None,
            event: InputEventKind::Keyboard {
                keycode: LinuxKeycode(30),
                state: ButtonState::Pressed,
            },
            time: 12,
        };
        state.borrow_mut().inbound.extend([
            DestinationEnvelope {
                session,
                message: DestinationMessage::Request(request.clone()),
            },
            DestinationEnvelope {
                session,
                message: DestinationMessage::input(input.clone()),
            },
        ]);

        adapter.drain_events(&mut ClientEventQueue::default());
        let mut effects = Vec::new();
        adapter.drain_effects(&mut effects);

        assert_eq!(
            effects,
            vec![
                ClientAdapterEffect::Request(request),
                ClientAdapterEffect::Input(input),
            ]
        );
        assert_eq!(state.borrow().accepted, 2);
    }

    #[test]
    fn cross_session_effect_fails_the_relay_without_applying_the_effect() {
        let (mut adapter, state, surface, session) = mapped_source();
        state.borrow_mut().inbound.push(DestinationEnvelope {
            session: HoistSessionId::new(session.raw() + 1),
            message: DestinationMessage::Request(ClientRequest::Surface(ClientSurfaceRequest {
                surface,
                kind: ClientSurfaceRequestKind::Close,
            })),
        });

        adapter.drain_events(&mut ClientEventQueue::default());
        let mut effects = Vec::new();
        adapter.drain_effects(&mut effects);

        assert!(state.borrow().disconnected);
        assert_eq!(state.borrow().accepted, 0);
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            ClientAdapterEffect::Request(ClientRequest::Surface(ClientSurfaceRequest {
                kind: ClientSurfaceRequestKind::Close,
                ..
            }))
        )));
    }

    #[test]
    fn late_effect_for_an_unmapped_surface_is_ignored_without_disconnect() {
        let (mut adapter, state, surface, session) = mapped_source();
        adapter.apply_command(ClientAdapterCommandEnvelope::new(
            ClientSourceId::new(8),
            HoistEndpointCommand::Unmap { source: surface },
        ));
        adapter.drain_effects(&mut Vec::new());
        state.borrow_mut().inbound.push(DestinationEnvelope {
            session,
            message: DestinationMessage::Request(ClientRequest::Surface(ClientSurfaceRequest {
                surface,
                kind: ClientSurfaceRequestKind::Close,
            })),
        });

        adapter.drain_events(&mut ClientEventQueue::default());
        let mut effects = Vec::new();
        adapter.drain_effects(&mut effects);

        assert!(!state.borrow().disconnected);
        assert_eq!(state.borrow().accepted, 0);
        assert!(effects.is_empty());
    }

    #[test]
    fn map_is_submitted_once_and_failed_withdraw_still_resets_scale() {
        let (mut adapter, state, surface, session) = mapped_source();
        adapter.apply_command(ClientAdapterCommandEnvelope::new(
            ClientSourceId::new(8),
            HoistEndpointCommand::Map {
                session,
                source: surface,
            },
        ));
        assert_eq!(
            state
                .borrow()
                .submitted
                .iter()
                .filter(|command| matches!(command, SourcePortCommand::MapSurface { .. }))
                .count(),
            1
        );

        state.borrow_mut().fail_withdraw = true;
        adapter.apply_command(ClientAdapterCommandEnvelope::new(
            ClientSourceId::new(8),
            HoistEndpointCommand::Unmap { source: surface },
        ));
        let mut effects = Vec::new();
        adapter.drain_effects(&mut effects);

        assert!(state.borrow().disconnected);
        assert!(
            effects.contains(&ClientAdapterEffect::Request(ClientRequest::Surface(
                ClientSurfaceRequest {
                    surface,
                    kind: ClientSurfaceRequestKind::SetPreferredScale { scale_120: None },
                },
            )))
        );
    }

    #[test]
    fn transport_loss_releases_remote_input_focus_and_scale() {
        let (mut adapter, state, surface, session) = mapped_source();
        state.borrow_mut().inbound.extend([
            DestinationEnvelope {
                session,
                message: DestinationMessage::Request(ClientRequest::Focus(ClientFocusRequest {
                    source: surface.source(),
                    surface: Some(surface),
                })),
            },
            DestinationEnvelope {
                session,
                message: DestinationMessage::input(ClientInputEvent {
                    target: ClientInputTarget::Keyboard { surface },
                    host_position: None,
                    event: InputEventKind::Keyboard {
                        keycode: LinuxKeycode(30),
                        state: ButtonState::Pressed,
                    },
                    time: 15,
                }),
            },
        ]);
        adapter.drain_events(&mut ClientEventQueue::default());
        adapter.drain_effects(&mut Vec::new());

        state.borrow_mut().fail_poll = true;
        adapter.drain_events(&mut ClientEventQueue::default());
        let mut effects = Vec::new();
        adapter.drain_effects(&mut effects);

        assert!(state.borrow().disconnected);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            ClientAdapterEffect::Input(ClientInputEvent {
                event: InputEventKind::Keyboard {
                    keycode: LinuxKeycode(30),
                    state: ButtonState::Released,
                },
                ..
            })
        )));
        assert!(
            effects.contains(&ClientAdapterEffect::Request(ClientRequest::Focus(
                ClientFocusRequest {
                    source: surface.source(),
                    surface: None,
                },
            )))
        );
        assert!(
            effects.contains(&ClientAdapterEffect::Request(ClientRequest::Surface(
                ClientSurfaceRequest {
                    surface,
                    kind: ClientSurfaceRequestKind::SetPreferredScale { scale_120: None },
                },
            )))
        );
    }
}
