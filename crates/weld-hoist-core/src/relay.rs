//! Shared source and destination relay policy over binding-owned ports.

use std::{
    collections::{HashMap, VecDeque},
    error::Error,
    fmt::Display,
    time::{Duration, Instant},
};

use weld_client::{
    ClientAdapter, ClientAdapterCommandEnvelope, ClientAdapterEffect, ClientBufferId,
    ClientEventQueue, ClientInputEvent, ClientInputTarget, ClientPresentationClaim,
    ClientPresentationUpdate, ClientRequest, ClientRouteAliasUpdate, ClientSourceDescriptor,
    ClientSourceId, ClientSurfaceCommit, ClientSurfaceEvent, ClientSurfaceEventKind,
    ClientSurfaceId, ClientSurfaceRequestKind, InputEventKind, InputPosition, KeyboardKeyState,
    LinuxButtonCode, LinuxKeycode, PointerGestureKind, RawScrollPhase, RawScrollSource,
};
use weld_hoist_protocol::{DestinationEnvelope, DestinationMessage, HoistSessionId};

use crate::{HoistEndpointCommand, relocated_surface};

pub type HoistPortError = Box<dyn Error + Send + Sync>;
pub type HoistPortResult<T> = Result<T, HoistPortError>;

pub enum SourcePortCommand {
    /// Local observation: a trusted focus request no longer names a mapped target.
    FocusCleared,
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
    /// Optional local service deadline; no transport roundtrip is implied.
    fn next_deadline(&self) -> Option<Instant> {
        None
    }
    /// Apply an authorized presentation preference and return the accepted
    /// upstream callback claim. Encoded ports may lower an explicit rate to
    /// their operating ceiling. Native forwarding uses the claim unchanged.
    fn set_presentation(
        &mut self,
        _surface: ClientSurfaceId,
        claim: ClientPresentationClaim,
    ) -> HoistPortResult<ClientPresentationClaim> {
        Ok(claim)
    }
    /// Whether authorization/bootstrap has completed and mapping may begin.
    fn ready(&self) -> bool {
        true
    }
    fn submit(&mut self, command: SourcePortCommand) -> HoistPortResult<()>;
    fn poll(&mut self) -> HoistPortResult<Vec<DestinationEnvelope>>;
    fn accept_destination(&mut self, envelope: &DestinationEnvelope) -> HoistPortResult<()>;
    /// Admit new work after the complete received control batch was validated.
    fn progress_after_destination(&mut self) -> HoistPortResult<()>;
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
    /// Local observation only; do not manufacture a wire request for an absent target.
    FocusCleared,
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
    metadata: Option<weld_client::ClientSurfaceMetadata>,
    sent_metadata: Option<weld_client::ClientSurfaceMetadata>,
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

/// Constructor-time consent policy; automatic admission cannot be toggled by commands.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SourceAdmission {
    #[default]
    Manual,
    /// Explicit consent to forward every mapped toplevel, including future ones.
    AllToplevels,
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
    admission: SourceAdmission,
    next_session: Option<u64>,
    admission_started: bool,
    presentations: HashMap<ClientSurfaceId, ClientPresentationClaim>,
    presentation_updates: Vec<ClientPresentationUpdate>,
}

impl SourceRelayAdapter {
    pub fn new(upstream_source: ClientSourceId, port: impl HoistSourcePort + 'static) -> Self {
        Self::with_admission(upstream_source, port, SourceAdmission::Manual)
    }

    pub fn with_admission(
        upstream_source: ClientSourceId,
        port: impl HoistSourcePort + 'static,
        admission: SourceAdmission,
    ) -> Self {
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
            admission,
            next_session: Some(1),
            admission_started: false,
            presentations: HashMap::new(),
            presentation_updates: Vec::new(),
        }
    }

    fn map(&mut self, session: HoistSessionId, source: ClientSurfaceId) {
        if self.failed
            || !self.port.ready()
            || source.source() != self.upstream_source
            || self.mappings.contains_key(&source)
        {
            return;
        }
        self.mappings.insert(source, session);
        let claim = self
            .cache
            .get(&source)
            .and_then(|cached| match cached.role {
                Some(weld_client::ClientSurfaceRole::Popup(popup)) => {
                    self.presentations.get(&popup.owner).copied()
                }
                _ => None,
            })
            .unwrap_or(ClientPresentationClaim::Active { rate: None });
        self.set_presentation(source, claim);
        tracing::info!(?source, ?session, "admitted hoist surface");
        if let Err(error) = self.port.submit(SourcePortCommand::MapSurface {
            session,
            surface: source,
        }) {
            self.fail(error);
            return;
        }
        if let Some((role, metadata, commit)) = self
            .cache
            .get(&source)
            .map(|cached| (cached.role, cached.metadata.clone(), cached.commit.clone()))
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
            if let Some(metadata) = metadata {
                if let Some(cached) = self.cache.get_mut(&source) {
                    cached.sent_metadata = Some(metadata.clone());
                }
                self.send_surface(
                    session,
                    ClientSurfaceEvent {
                        surface: source,
                        kind: ClientSurfaceEventKind::Metadata(metadata),
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
            .filter(|(_, cached)| {
                self.admission == SourceAdmission::Manual
                    || cached.commit.as_ref().is_some_and(|commit| commit.mapped)
            })
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

    fn admit(&mut self, source: ClientSurfaceId) {
        if self.admission != SourceAdmission::AllToplevels
            || self.failed
            || !self.port.ready()
            || self.mappings.contains_key(&source)
        {
            return;
        }
        let Some(cached) = self.cache.get(&source) else {
            return;
        };
        if !cached.commit.as_ref().is_some_and(|commit| commit.mapped) {
            return;
        }
        let session = match cached.role {
            Some(weld_client::ClientSurfaceRole::Toplevel(_)) => {
                let Some(next) = self.next_session else {
                    self.fail("hoist session identifiers exhausted");
                    return;
                };
                self.next_session = next.checked_add(1);
                HoistSessionId::new(next)
            }
            Some(weld_client::ClientSurfaceRole::Popup(popup)) => {
                let Some(session) = self.mappings.get(&popup.owner) else {
                    return;
                };
                *session
            }
            None => return,
        };
        self.map(session, source);
    }

    fn unmap(&mut self, source: ClientSurfaceId) {
        let Some(session) = self.mappings.remove(&source) else {
            return;
        };
        self.release_presentation(source);
        self.pending_cursors.retain(|surface| *surface != source);
        if let Some(cached) = self.cache.get_mut(&source) {
            cached.sent_cursor = None;
            cached.sent_metadata = None;
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
            ClientSurfaceEventKind::Metadata(metadata) => {
                let cached = self.cache.entry(source).or_default();
                cached.metadata = Some(metadata.clone());
                if let Some(session) = self.mappings.get(&source).copied()
                    && cached.sent_metadata.as_ref() != Some(metadata)
                {
                    cached.sent_metadata = Some(metadata.clone());
                    self.send_surface(session, event.clone());
                }
            }
            ClientSurfaceEventKind::Role(role) => {
                self.cache.entry(source).or_default().role = Some(*role);
                // Already admitted popups still publish position and stack changes.
                // Only the initial map replays the cached role itself.
                if let Some(session) = self.mappings.get(&source).copied() {
                    self.send_surface(session, event.clone());
                } else if self.admission == SourceAdmission::Manual
                    && let weld_client::ClientSurfaceRole::Popup(popup) = role
                    && let Some(session) = self.mappings.get(&popup.owner).copied()
                {
                    self.map(session, source);
                } else {
                    self.admit(source);
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
                } else {
                    // map() replays the just-cached commit: do not send it twice.
                    self.admit(source);
                }
            }
            ClientSurfaceEventKind::Interaction(_) => {
                if let Some(session) = self.mappings.get(&source).copied() {
                    self.send_surface(session, event.clone());
                }
            }
            ClientSurfaceEventKind::Destroyed => {
                let children = self
                    .cache
                    .iter()
                    .filter_map(|(surface, cached)| match cached.role {
                        Some(weld_client::ClientSurfaceRole::Popup(popup))
                            if popup.owner == source =>
                        {
                            Some(*surface)
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                for child in children {
                    self.unmap(child);
                }
                self.release_presentation(source);
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
        if self.admission == SourceAdmission::AllToplevels
            && !self.admission_started
            && self.port.ready()
        {
            self.admission_started = true;
            let surfaces = self.cache.keys().copied().collect::<Vec<_>>();
            for surface in surfaces {
                self.admit(surface);
            }
        }
        for envelope in envelopes {
            if !self.accept_destination(envelope) {
                break;
            }
        }
        if !self.failed
            && let Err(error) = self.port.progress_after_destination()
        {
            self.fail(error);
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
                    // A live session on this peer may clear attention even if
                    // its newly requested target is not mapped yet. Unknown
                    // sessions cannot change another session's attention.
                    if matches!(
                        &envelope.message,
                        DestinationMessage::Request(ClientRequest::Focus(_))
                    ) && self
                        .mappings
                        .values()
                        .any(|session| *session == envelope.session)
                        && let Err(error) = self.port.submit(SourcePortCommand::FocusCleared)
                    {
                        self.fail(error);
                        return false;
                    }
                    tracing::debug!(?surface, session = ?envelope.session,
                        message_kind = envelope.message.kind(),
                        "ignored destination message for an unmapped surface");
                    return true;
                }
            }
        }
        if let DestinationMessage::Input(input) = &envelope.message
            && !self.remote_input.accepts_input(&input.target, &input.event)
        {
            return true;
        }
        if let Err(error) = self.port.accept_destination(&envelope) {
            self.fail(error);
            return false;
        }
        match envelope.message {
            DestinationMessage::Request(request) => {
                match &request {
                    ClientRequest::Surface(surface) => match surface.kind {
                        ClientSurfaceRequestKind::SetPresentation { rate } => {
                            self.set_presentation(
                                surface.surface,
                                rate.map_or(ClientPresentationClaim::Paused, |rate| {
                                    ClientPresentationClaim::Active { rate: Some(rate) }
                                }),
                            );
                            return true;
                        }
                        ClientSurfaceRequestKind::Close
                        | ClientSurfaceRequestKind::Configure { .. }
                        | ClientSurfaceRequestKind::SetOutputs { .. }
                        | ClientSurfaceRequestKind::SetPreferredScale { .. } => {}
                    },
                    ClientRequest::Focus(_) | ClientRequest::ClearFocus => {}
                }
                self.remote_input.observe_request(&request);
                self.effects.push(ClientAdapterEffect::Request(request));
            }
            DestinationMessage::Input(input) => {
                let input = input.into_client_event();
                if self.remote_input.observe_input(&input) {
                    self.effects.push(ClientAdapterEffect::Input(input));
                }
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
        for (surface, _) in self.presentations.drain() {
            self.presentation_updates.push(ClientPresentationUpdate {
                surface,
                claim: ClientPresentationClaim::Release,
            });
        }
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
    fn set_presentation(&mut self, source: ClientSurfaceId, claim: ClientPresentationClaim) {
        if self.failed || !self.mappings.contains_key(&source) {
            return;
        }
        if self.presentations.insert(source, claim) == Some(claim) {
            return;
        }
        let accepted = match self.port.set_presentation(source, claim) {
            Ok(accepted) => accepted,
            Err(error) => {
                self.fail(error);
                return;
            }
        };
        self.presentation_updates.push(ClientPresentationUpdate {
            surface: source,
            claim: accepted,
        });
        let children = self
            .cache
            .iter()
            .filter_map(|(surface, cached)| match cached.role {
                Some(weld_client::ClientSurfaceRole::Popup(popup)) if popup.owner == source => {
                    Some(*surface)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for child in children {
            self.set_presentation(child, claim);
        }
    }

    fn release_presentation(&mut self, surface: ClientSurfaceId) {
        if self.presentations.remove(&surface).is_some() {
            self.presentation_updates.push(ClientPresentationUpdate {
                surface,
                claim: ClientPresentationClaim::Release,
            });
        }
    }

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
    fn next_deadline(&self) -> Option<Instant> {
        if self.failed {
            None
        } else {
            self.port.next_deadline()
        }
    }
    fn presentation_source(&self) -> Option<ClientSourceId> {
        Some(self.upstream_source)
    }

    fn drain_presentation_claims(&mut self, updates: &mut Vec<ClientPresentationUpdate>) {
        updates.append(&mut self.presentation_updates);
    }
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
        if self.admission != SourceAdmission::Manual {
            return;
        }
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
            if matches!(&request, ClientRequest::Focus(focus) if focus.source == self.descriptor.id)
                && let Err(error) = self.port.submit(DestinationPortCommand::FocusCleared)
            {
                self.fail(error);
            }
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
        if !self.input.observe_input(&event) {
            return;
        }
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
    keys: HashMap<LinuxKeycode, RemoteKeyCapture>,
    buttons: HashMap<LinuxButtonCode, (ClientInputTarget, Option<InputPosition>)>,
    gestures: Vec<(PointerGestureKind, ClientInputTarget)>,
    finger_scroll: Option<RemoteFingerScroll>,
    keyboard_focus: Option<ClientSurfaceId>,
    last_time: u32,
}

struct RemoteKeyCapture {
    target: ClientInputTarget,
    repeat_allowed: bool,
}

struct RemoteFingerScroll {
    target: ClientInputTarget,
    horizontal_active: bool,
    vertical_active: bool,
}

impl RemoteInputState {
    // Eligibility must not add releases to the teardown ledger before the port
    // accepts delivery. In particular, a failed press was never forwarded.
    fn accepts_input(&self, target: &ClientInputTarget, event: &InputEventKind) -> bool {
        match event {
            InputEventKind::Keyboard {
                keycode,
                state: KeyboardKeyState::Repeated,
            } => self
                .keys
                .get(keycode)
                .is_some_and(|capture| capture.repeat_allowed && capture.target == *target),
            _ => true,
        }
    }

    fn observe_request(&mut self, request: &ClientRequest) {
        if let ClientRequest::Focus(focus) = request {
            if self.keyboard_focus != focus.surface {
                for capture in self.keys.values_mut() {
                    capture.repeat_allowed = false;
                }
            }
            self.keyboard_focus = focus.surface;
        }
    }

    fn observe_input(&mut self, input: &ClientInputEvent) -> bool {
        if let InputEventKind::Keyboard {
            state: KeyboardKeyState::Repeated,
            ..
        } = input.event
        {
            let eligible = self.accepts_input(&input.target, &input.event);
            if eligible {
                self.last_time = input.time;
            }
            return eligible;
        }
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
                KeyboardKeyState::Pressed => {
                    self.keys.insert(
                        *keycode,
                        RemoteKeyCapture {
                            target: input.target,
                            repeat_allowed: true,
                        },
                    );
                }
                KeyboardKeyState::Released => {
                    self.keys.remove(keycode);
                }
                KeyboardKeyState::Repeated => {}
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
        true
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
                .extract_if(|_, capture| should_release(capture.target.surface()))
                .map(|(keycode, capture)| {
                    ClientAdapterEffect::Input(ClientInputEvent {
                        target: capture.target,
                        host_position: None,
                        event: InputEventKind::Keyboard {
                            keycode,
                            state: KeyboardKeyState::Released,
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
    mod presentation_tests;
    use std::{cell::RefCell, fmt, rc::Rc};

    use weld_client::{
        ButtonState, ClientAdapter, ClientAdapterCommandEnvelope, ClientFocusRequest, ClientId,
        ClientInputEvent, ClientInputTarget, ClientKeyboardRoute, ClientProvenance, ClientRequest,
        ClientRuntime, ClientRuntimeAdapter, ClientSurfaceRequest, ClientSurfaceRequestKind,
        ClientSurfaceRole, InputEventKind, LinuxKeycode, LogicalPoint, PopupState,
        RuntimeInputEvent, RuntimeInputEventKind,
    };

    use super::*;

    #[test]
    fn relay_forwards_repeats_without_growing_the_release_ledger() {
        let (mut relay, port, surface, session) = mapped_source();
        let input = |state| DestinationEnvelope {
            session,
            message: DestinationMessage::input(ClientInputEvent {
                target: ClientInputTarget::Keyboard { surface },
                host_position: None,
                event: InputEventKind::Keyboard {
                    keycode: LinuxKeycode(30),
                    state,
                },
                time: if state == KeyboardKeyState::Repeated {
                    100
                } else {
                    10
                },
            }),
        };
        relay.effects.clear();
        assert!(relay.accept_destination(input(KeyboardKeyState::Repeated)));
        assert!(relay.effects.is_empty());
        assert_eq!(
            port.borrow().accepted,
            0,
            "invalid repeats never reach port activity"
        );
        assert_eq!(
            relay.remote_input.last_time, 0,
            "invalid repeat does not advance input time"
        );
        for state in [
            KeyboardKeyState::Pressed,
            KeyboardKeyState::Repeated,
            KeyboardKeyState::Repeated,
        ] {
            assert!(relay.accept_destination(input(state)));
        }
        assert_eq!(relay.effects.len(), 3);
        assert_eq!(relay.remote_input.keys.len(), 1);
        relay.effects.clear();
        relay.fail("test disconnect");
        let releases = relay
            .effects
            .iter()
            .filter(|effect| {
                matches!(
                    effect,
                    ClientAdapterEffect::Input(ClientInputEvent {
                        event: InputEventKind::Keyboard {
                            state: KeyboardKeyState::Released,
                            ..
                        },
                        ..
                    })
                )
            })
            .count();
        assert_eq!(releases, 1);
        assert!(
            relay.effects.iter().any(|effect| matches!(
                effect,
                ClientAdapterEffect::Input(ClientInputEvent {
                    event: InputEventKind::Keyboard {
                        state: KeyboardKeyState::Released,
                        ..
                    },
                    time: 100,
                    ..
                })
            )),
            "release uses the latest accepted repeat timestamp"
        );
    }

    #[test]
    fn relay_focus_change_cancels_repeat_but_keeps_the_release() {
        let mut input = RemoteInputState::default();
        let surface = surface(ClientSourceId::new(1), 1);
        let focus = |surface| {
            ClientRequest::Focus(ClientFocusRequest {
                source: ClientSourceId::new(1),
                surface,
            })
        };
        input.observe_request(&focus(Some(surface)));
        let mut event = key_input(surface, 30, ButtonState::Pressed);
        assert!(input.observe_input(&event));
        event.event = InputEventKind::Keyboard {
            keycode: LinuxKeycode(30),
            state: KeyboardKeyState::Repeated,
        };
        assert!(input.observe_input(&event));
        input.observe_request(&focus(None));
        input.observe_request(&focus(Some(surface)));
        assert!(!input.observe_input(&event));
        assert_eq!(input.keys.len(), 1);
        assert!(input.observe_input(&key_input(surface, 30, ButtonState::Released)));
        assert!(input.keys.is_empty());
    }

    #[derive(Default)]
    struct FakeSourceState {
        not_ready: bool,
        inbound: Vec<DestinationEnvelope>,
        submitted: Vec<SourcePortCommand>,
        accepted: usize,
        progress_counts: Vec<usize>,
        fail_poll: bool,
        fail_accept: bool,
        fail_withdraw: bool,
        disconnected: bool,
        ceiling: Option<weld_client::PresentationRate>,
        presentations: Vec<(ClientSurfaceId, ClientPresentationClaim)>,
        deadline: Option<Instant>,
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
        fn next_deadline(&self) -> Option<Instant> {
            self.0.borrow().deadline
        }
        fn set_presentation(
            &mut self,
            surface: ClientSurfaceId,
            claim: ClientPresentationClaim,
        ) -> HoistPortResult<ClientPresentationClaim> {
            let mut state = self.0.borrow_mut();
            state.presentations.push((surface, claim));
            Ok(match claim {
                ClientPresentationClaim::Active { rate: Some(rate) } => {
                    ClientPresentationClaim::Active {
                        rate: Some(state.ceiling.map_or(rate, |ceiling| ceiling.min(rate))),
                    }
                }
                other => other,
            })
        }
        fn ready(&self) -> bool {
            !self.0.borrow().not_ready
        }
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
            if self.0.borrow().fail_accept {
                return Err(Box::new(FakePortFailure));
            }
            self.0.borrow_mut().accepted += 1;
            Ok(())
        }

        fn effects_drained(&mut self) {}

        fn progress_after_destination(&mut self) -> HoistPortResult<()> {
            let mut state = self.0.borrow_mut();
            let accepted = state.accepted;
            state.progress_counts.push(accepted);
            Ok(())
        }

        fn disconnect(&mut self) {
            self.0.borrow_mut().disconnected = true;
        }
    }

    fn surface(source: ClientSourceId, local: u64) -> ClientSurfaceId {
        ClientSurfaceId::new(ClientId::new(source, 1), local)
    }

    fn auto_source() -> (SourceRelayAdapter, Rc<RefCell<FakeSourceState>>) {
        let port = Rc::new(RefCell::new(FakeSourceState {
            not_ready: true,
            ..Default::default()
        }));
        (
            SourceRelayAdapter::with_admission(
                ClientSourceId::new(1),
                FakeSourcePort(port.clone()),
                SourceAdmission::AllToplevels,
            ),
            port,
        )
    }

    fn top_role(parent: Option<ClientSurfaceId>) -> ClientSurfaceEventKind {
        ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(weld_client::ToplevelState {
            parent,
            decoration: weld_client::WindowDecoration::ServerSide,
        }))
    }

    fn commit(revision: u64, mapped: bool) -> ClientSurfaceEventKind {
        ClientSurfaceEventKind::Commit(ClientSurfaceCommit {
            revision: weld_client::ClientCommitRevision::new(revision),
            alpha_mode: weld_client::SurfaceAlphaMode::Preserved,
            mapped,
            root: None,
            window_geometry: None,
            overlays: Vec::new(),
            inputs: Vec::new(),
            buffers: Vec::new(),
        })
    }

    fn observe(
        relay: &mut SourceRelayAdapter,
        surface: ClientSurfaceId,
        kind: ClientSurfaceEventKind,
    ) {
        relay.observe_event(&ClientSurfaceEvent { surface, kind });
    }

    #[test]
    fn automatic_admission_replays_only_latest_state_once_after_authorization() {
        let (mut relay, port) = auto_source();
        let window = surface(ClientSourceId::new(1), 1);
        observe(&mut relay, window, top_role(None));
        for revision in 1..=60 {
            observe(&mut relay, window, commit(revision, true));
            relay.poll();
        }
        assert!(port.borrow().submitted.is_empty());
        assert!(relay.mappings.is_empty());
        port.borrow_mut().not_ready = false;
        relay.poll();
        relay.poll();
        let state = port.borrow();
        assert_eq!(state.submitted.len(), 3, "one map, role and latest commit");
        assert!(matches!(&state.submitted[2], SourcePortCommand::Surface {
            event: ClientSurfaceEvent { kind: ClientSurfaceEventKind::Commit(commit), .. }, ..
        } if commit.revision == weld_client::ClientCommitRevision::new(60)));
    }

    #[test]
    fn metadata_replays_after_role_and_unchanged_labels_are_not_resent() {
        let (mut relay, port) = auto_source();
        let window = surface(ClientSourceId::new(1), 1);
        let label = |title: &str| {
            ClientSurfaceEventKind::Metadata(
                weld_client::ClientSurfaceMetadata::new("test.app".into(), title.into()).unwrap(),
            )
        };
        observe(&mut relay, window, top_role(None));
        observe(&mut relay, window, label("old"));
        observe(&mut relay, window, label("current"));
        observe(&mut relay, window, commit(1, true));
        assert!(port.borrow().submitted.is_empty());
        port.borrow_mut().not_ready = false;
        relay.poll();
        let kinds: Vec<_> = port
            .borrow()
            .submitted
            .iter()
            .filter_map(|command| match command {
                SourcePortCommand::Surface { event, .. } => Some(match &event.kind {
                    ClientSurfaceEventKind::Role(_) => "role",
                    ClientSurfaceEventKind::Metadata(metadata) => {
                        assert_eq!(metadata.title(), "current");
                        "metadata"
                    }
                    ClientSurfaceEventKind::Commit(_) => "commit",
                    _ => "other",
                }),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, ["role", "metadata", "commit"]);
        let count = port.borrow().submitted.len();
        for _ in 0..100 {
            observe(&mut relay, window, label("current"));
        }
        assert_eq!(port.borrow().submitted.len(), count);
        observe(&mut relay, window, label("changed"));
        assert_eq!(port.borrow().submitted.len(), count + 1);
    }

    #[test]
    fn pending_admission_retains_only_the_current_buffer_use_per_layer() {
        let (mut relay, port) = auto_source();
        let source = ClientSourceId::new(1);
        let window = surface(source, 1);
        let released = Rc::new(RefCell::new(Vec::new()));
        let metadata = weld_client::ClientBufferMetadata::new(weld_client::Extent::new(1, 1), true);
        observe(&mut relay, window, top_role(None));
        for revision in 1..=60 {
            let released = released.clone();
            let lease = weld_client::ClientBufferLease::new(
                ClientBufferId::new(source, revision),
                weld_client::ClientBufferUseId::new(source, revision),
                metadata,
                Rc::new(()),
                move |_| released.borrow_mut().push(revision),
            )
            .expect("owned fixture use");
            let ClientSurfaceEventKind::Commit(mut changed) = commit(revision, true) else {
                panic!("commit fixture");
            };
            changed.buffers.push(weld_client::SurfaceBufferUpdate {
                layer: weld_client::SurfaceLayerId::new(1),
                change: weld_client::SurfaceBufferChange::Replaced {
                    metadata,
                    buffer: lease,
                },
            });
            observe(&mut relay, window, ClientSurfaceEventKind::Commit(changed));
        }
        assert_eq!(*released.borrow(), (1..60).collect::<Vec<_>>());
        assert!(port.borrow().submitted.is_empty());
        // A later metadata-only commit must not lose the cached pixels.
        let ClientSurfaceEventKind::Commit(mut retained) = commit(61, true) else {
            panic!("commit fixture");
        };
        retained.buffers.push(weld_client::SurfaceBufferUpdate {
            layer: weld_client::SurfaceLayerId::new(1),
            change: weld_client::SurfaceBufferChange::Retained { metadata },
        });
        observe(&mut relay, window, ClientSurfaceEventKind::Commit(retained));
        assert_eq!(released.borrow().len(), 59);
        port.borrow_mut().not_ready = false;
        relay.poll();
        observe(&mut relay, window, ClientSurfaceEventKind::Destroyed);
        assert_eq!(
            released.borrow().len(),
            59,
            "port still owns the replayed use"
        );
        port.borrow_mut().submitted.clear();
        assert_eq!(released.borrow().len(), 60);
    }

    #[test]
    fn automatic_admission_maps_dialogs_separately_and_popups_with_their_owner() {
        let (mut relay, port) = auto_source();
        let owner = surface(ClientSourceId::new(1), 1);
        let popup = surface(ClientSourceId::new(1), 2);
        let dialog = surface(ClientSourceId::new(1), 3);
        observe(
            &mut relay,
            popup,
            ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(PopupState {
                owner,
                position: LogicalPoint::new(20.0, 30.0),
                stack_index: 0,
            })),
        );
        observe(&mut relay, popup, commit(1, true));
        observe(&mut relay, owner, top_role(None));
        observe(&mut relay, owner, commit(1, true));
        port.borrow_mut().not_ready = false;
        relay.poll();
        assert_eq!(relay.mappings.get(&owner), relay.mappings.get(&popup));
        assert!(relay.mappings.contains_key(&owner));
        // Commit before role is also supported, including after the initial sweep.
        observe(&mut relay, dialog, commit(1, true));
        assert!(!relay.mappings.contains_key(&dialog));
        port.borrow_mut().submitted.clear();
        observe(&mut relay, dialog, top_role(Some(owner)));
        assert_ne!(relay.mappings.get(&owner), relay.mappings.get(&dialog));
        assert_eq!(
            port.borrow().submitted.len(),
            3,
            "admitting commit emitted exactly once"
        );
        observe(&mut relay, popup, ClientSurfaceEventKind::Destroyed);
        assert!(!relay.mappings.contains_key(&popup));
        assert!(!relay.cache.contains_key(&popup));
        assert!(relay.mappings.contains_key(&owner));
    }

    #[test]
    fn automatic_admission_ignores_unmapped_destroyed_foreign_and_manual_targets() {
        let (mut relay, port) = auto_source();
        let unmapped = surface(ClientSourceId::new(1), 1);
        let destroyed = surface(ClientSourceId::new(1), 2);
        let foreign = surface(ClientSourceId::new(2), 1);
        for window in [unmapped, destroyed, foreign] {
            observe(&mut relay, window, top_role(None));
            observe(&mut relay, window, commit(1, window != unmapped));
        }
        observe(&mut relay, destroyed, ClientSurfaceEventKind::Destroyed);
        port.borrow_mut().not_ready = false;
        relay.apply_command(ClientAdapterCommandEnvelope::new(
            ClientSourceId::new(1),
            HoistEndpointCommand::Map {
                session: HoistSessionId::new(99),
                source: unmapped,
            },
        ));
        relay.poll();
        assert!(relay.mappings.is_empty());
        observe(&mut relay, unmapped, commit(2, true));
        assert_eq!(relay.mappings.get(&unmapped), Some(&HoistSessionId::new(1)));
    }

    #[test]
    fn failed_admission_never_restarts_or_wraps_session_ids() {
        let (mut relay, port) = auto_source();
        port.borrow_mut().not_ready = false;
        relay.next_session = Some(u64::MAX);
        for index in 1..=2 {
            let window = surface(ClientSourceId::new(1), index);
            observe(&mut relay, window, top_role(None));
            observe(&mut relay, window, commit(1, true));
        }
        assert!(relay.failed);
        let count = port.borrow().submitted.len();
        let third = surface(ClientSourceId::new(1), 3);
        observe(&mut relay, third, top_role(None));
        observe(&mut relay, third, commit(1, true));
        relay.poll();
        assert_eq!(port.borrow().submitted.len(), count);
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
    fn progress_runs_after_all_accepted_input_and_on_empty_polls_but_not_failure() {
        let (mut relay, port, surface, session) = mapped_source();
        for key in [30, 31] {
            port.borrow_mut().inbound.push(DestinationEnvelope {
                session,
                message: DestinationMessage::input(key_input(surface, key, ButtonState::Pressed)),
            });
        }
        relay.poll();
        relay.poll();
        assert_eq!(port.borrow().progress_counts, vec![2, 2]);
        port.borrow_mut().fail_poll = true;
        relay.poll();
        assert_eq!(port.borrow().progress_counts, vec![2, 2]);
        assert!(port.borrow().disconnected);
    }

    #[test]
    fn failed_press_does_not_generate_a_release_for_an_undelivered_key() {
        let (mut relay, port, surface, session) = mapped_source();
        relay.effects.clear();
        port.borrow_mut().fail_accept = true;
        assert!(!relay.accept_destination(DestinationEnvelope {
            session,
            message: DestinationMessage::input(key_input(surface, 30, ButtonState::Pressed))
        }));
        assert!(relay.remote_input.keys.is_empty());
        assert!(
            !relay
                .effects
                .iter()
                .any(|effect| matches!(effect, ClientAdapterEffect::Input(_)))
        );
    }

    #[test]
    fn destination_unmapped_focus_is_a_local_observation_not_a_wire_request() {
        let upstream = ClientSourceId::new(1);
        let destination = ClientSourceId::new(2);
        let port = Rc::new(RefCell::new(FakeDestinationState::default()));
        let mut relay = DestinationRelayAdapter::new(
            upstream,
            ClientSourceDescriptor::new(destination, ClientProvenance::Relocated),
            FakeDestinationPort(port.clone()),
        );
        relay.apply_request(ClientRequest::Focus(ClientFocusRequest {
            source: destination,
            surface: Some(surface(destination, 99)),
        }));
        assert_eq!(port.borrow().outbound.len(), 1);
        assert!(matches!(
            port.borrow().outbound[0],
            DestinationPortCommand::FocusCleared
        ));
        assert!(relay.input.keyboard_focus.is_none());
    }

    #[test]
    fn only_a_live_peer_session_can_clear_activity_for_an_unmapped_focus_target() {
        let (mut relay, port, mapped, session) = mapped_source();
        let missing = surface(mapped.source(), 99);
        for (session, expected) in [(HoistSessionId::new(999), 0), (session, 1)] {
            assert!(relay.accept_destination(DestinationEnvelope {
                session,
                message: DestinationMessage::Request(ClientRequest::Focus(ClientFocusRequest {
                    source: mapped.source(),
                    surface: Some(missing)
                }))
            }));
            assert_eq!(
                port.borrow()
                    .submitted
                    .iter()
                    .filter(|command| matches!(command, SourcePortCommand::FocusCleared))
                    .count(),
                expected
            );
        }
        assert_eq!(
            port.borrow().accepted,
            0,
            "unknown focus is never installed"
        );
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
                state: state.into(),
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
                            state: weld_client::KeyboardKeyState::Released,
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
                state: weld_client::KeyboardKeyState::Pressed,
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
                        state: weld_client::KeyboardKeyState::Pressed,
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
                    state: weld_client::KeyboardKeyState::Released,
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
