//! Shared source and destination relay policy over binding-owned ports.

use std::{
    collections::{HashMap, HashSet},
    error::Error,
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
        }
    }

    fn map(&mut self, session: HoistSessionId, source: ClientSurfaceId) {
        if source.source() != self.upstream_source || self.mappings.contains_key(&source) {
            return;
        }
        self.mappings.insert(source, session);
        if self
            .port
            .submit(SourcePortCommand::MapSurface {
                session,
                surface: source,
            })
            .is_err()
        {
            self.fail();
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
        self.effects
            .push(ClientAdapterEffect::Request(ClientRequest::Surface(
                weld_client::ClientSurfaceRequest {
                    surface: source,
                    kind: ClientSurfaceRequestKind::SetPreferredScale { scale_120: None },
                },
            )));
        if self
            .port
            .submit(SourcePortCommand::WithdrawSurface {
                session,
                surface: source,
            })
            .is_err()
        {
            self.fail();
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
                if let Some(session) = self.mappings.remove(&source) {
                    self.send_surface(session, event.clone());
                }
                self.cache.remove(&source);
            }
        }
    }

    fn send_surface(&mut self, session: HoistSessionId, event: ClientSurfaceEvent) {
        if self.failed {
            return;
        }
        if self
            .port
            .submit(SourcePortCommand::Surface { session, event })
            .is_err()
        {
            self.fail();
        }
    }

    fn poll(&mut self) {
        if self.failed {
            return;
        }
        let envelopes = match self.port.poll() {
            Ok(envelopes) => envelopes,
            Err(_) => {
                self.fail();
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
        let target = destination_message_surface(&envelope.message);
        if let Some(surface) = target {
            match self.mappings.get(&surface).copied() {
                Some(session) if session == envelope.session => {}
                Some(_) => {
                    self.fail();
                    return false;
                }
                None => return true,
            }
        }
        if self.port.accept_destination(&envelope).is_err() {
            self.fail();
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
            DestinationMessage::BufferReleased { .. }
            | DestinationMessage::Reclaim
            | DestinationMessage::EncodedCommitFinished { .. } => {}
        }
        true
    }

    fn fail(&mut self) {
        if self.failed {
            return;
        }
        self.failed = true;
        self.port.disconnect();
        self.effects.extend(self.remote_input.release_effects(
            self.upstream_source,
            self.mappings.keys().copied().collect(),
        ));
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

impl ClientAdapter for SourceRelayAdapter {
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
            && self
                .port
                .submit(SourcePortCommand::RetireUpstreamBuffer(buffer))
                .is_err()
        {
            self.fail();
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
    keyboard_focus: Option<ClientSurfaceId>,
    failed: bool,
    port: Box<dyn HoistDestinationPort>,
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
            keyboard_focus: None,
            failed: false,
            port: Box::new(port),
        }
    }

    fn poll(&mut self) {
        if self.failed {
            return;
        }
        let records = match self.port.poll() {
            Ok(records) => records,
            Err(_) => {
                self.fail();
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
            DestinationPortEvent::MappedSurface(source) => {
                if !self.map_surface(record.session, source) {
                    return false;
                }
            }
            DestinationPortEvent::Surface(event) => {
                let source = event.surface;
                if source.source() != self.upstream_source {
                    self.fail();
                    return false;
                }
                if self.sessions.get(&source).copied() != Some(record.session) {
                    self.fail();
                    return false;
                }
                if matches!(event.kind, ClientSurfaceEventKind::Destroyed) {
                    self.destroy_surface(source, true);
                    return true;
                }
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
            self.fail();
            return false;
        }
        match self.sessions.get(&source).copied() {
            Some(current) if current == session => return true,
            Some(_) => {
                self.fail();
                return false;
            }
            None => {}
        }
        self.sessions.insert(source, session);
        let destination = relocated_surface(self.descriptor.id, source);
        if self
            .port
            .submit(DestinationPortCommand::RouteMapped {
                source,
                destination,
            })
            .is_err()
        {
            self.fail();
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
        if self.sessions.remove(&source).is_none() {
            return;
        }
        self.roles.remove(&source);
        let destination = relocated_surface(self.descriptor.id, source);
        if self.keyboard_focus == Some(destination) {
            self.keyboard_focus = None;
        }
        if notify_port
            && self
                .port
                .submit(DestinationPortCommand::RouteUnmapped { destination })
                .is_err()
        {
            self.fail();
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

    fn fail(&mut self) {
        if self.failed {
            return;
        }
        self.failed = true;
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
        if self
            .port
            .submit(DestinationPortCommand::Message(DestinationEnvelope {
                session,
                message,
            }))
            .is_err()
        {
            self.fail();
        }
    }
}

impl ClientAdapter for DestinationRelayAdapter {
    fn drain_events(&mut self, events: &mut ClientEventQueue) {
        self.poll();
        while let Some(event) = self.events.pop_front() {
            events.push(event);
        }
    }

    fn apply_request(&mut self, mut request: ClientRequest) {
        let destination = match &request {
            ClientRequest::Focus(focus) if focus.surface.is_none() => self.keyboard_focus,
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
        match &mut request {
            ClientRequest::Focus(focus) if focus.surface.is_none() => {
                focus.source = self.upstream_source;
                self.keyboard_focus = None;
            }
            ClientRequest::Focus(_) => {
                rewrite_request_surface(&mut request, source);
                self.keyboard_focus = Some(destination);
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
    fn host_focus_lost(&mut self, _time: u32) {}

    fn drain_route_alias_updates(&mut self, updates: &mut Vec<ClientRouteAliasUpdate>) {
        self.port.drain_route_alias_updates(updates);
    }
}

fn destination_message_surface(message: &DestinationMessage) -> Option<ClientSurfaceId> {
    match message {
        DestinationMessage::Request(request) => request_surface(request),
        DestinationMessage::Input(input) => Some(input.target.surface()),
        DestinationMessage::EncodedCommitFinished { surface, .. } => Some(*surface),
        DestinationMessage::BufferReleased { .. } | DestinationMessage::Reclaim => None,
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
            InputEventKind::PointerMotion { .. }
            | InputEventKind::PointerLeft { .. }
            | InputEventKind::PointerAxis { .. } => {}
        }
    }

    fn release_effects(
        &mut self,
        source: ClientSourceId,
        mapped_surfaces: HashSet<ClientSurfaceId>,
    ) -> Vec<ClientAdapterEffect> {
        let mut effects = Vec::new();
        let time = self.last_time;
        effects.extend(self.buttons.drain().map(|(button, (target, position))| {
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
        }));
        effects.extend(self.keys.drain().map(|(keycode, target)| {
            ClientAdapterEffect::Input(ClientInputEvent {
                target,
                host_position: None,
                event: InputEventKind::Keyboard {
                    keycode,
                    state: weld_client::ButtonState::Released,
                },
                time,
            })
        }));
        effects.extend(self.gestures.drain(..).map(|(kind, target)| {
            ClientAdapterEffect::Input(ClientInputEvent {
                target,
                host_position: None,
                event: InputEventKind::PointerGesture {
                    gesture: kind.cancelled(),
                },
                time,
            })
        }));
        if let Some(scroll) = self.finger_scroll.take() {
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
        if self
            .keyboard_focus
            .take()
            .is_some_and(|surface| mapped_surfaces.contains(&surface))
        {
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
        ClientInputEvent, ClientInputTarget, ClientProvenance, ClientRequest, ClientSurfaceRequest,
        ClientSurfaceRequestKind, ClientSurfaceRole, InputEventKind, LinuxKeycode, LogicalPoint,
        PopupState,
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

    struct FakeDestinationPort;

    impl HoistDestinationPort for FakeDestinationPort {
        fn poll(&mut self) -> HoistPortResult<Vec<DestinationPortRecord>> {
            Ok(Vec::new())
        }

        fn submit(&mut self, _command: DestinationPortCommand) -> HoistPortResult<()> {
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
            FakeDestinationPort,
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
