use std::{
    collections::{HashMap, HashSet},
    os::fd::OwnedFd,
    rc::Rc,
};

use tracing::{error, warn};
use weld_client::{
    ClientAdapter, ClientAdapterCommandEnvelope, ClientAdapterEffect, ClientAdapterRegistration,
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, ClientEventQueue,
    ClientInputEvent, ClientInputTarget, ClientProvenance, ClientRequest, ClientSourceDescriptor,
    ClientSourceId, ClientSurfaceCommit, ClientSurfaceEvent, ClientSurfaceEventKind,
    ClientSurfaceId, ClientSurfaceRequestKind, ControlOnlyClientImporter, InputEventKind,
    InputPosition, LinuxButtonCode, LinuxKeycode, PointerGestureKind, RawScrollPhase,
    RawScrollSource, WireClientSurfaceEvent, WireClientSurfaceEventKind, WireSurfaceBufferChange,
};
use weld_core::dmabuf::{
    DirectClientBufferAccess, DirectClientBufferImporter, DmabufAccess, DmabufContext,
};
use weld_hoist_core::{HoistEndpoint, HoistEndpointCommand, HoistSessionId, relocated_surface};

use crate::{
    LocalBuffer, LocalBufferContent, LocalDestinationMessage, LocalDestinationPacket,
    LocalPacketConnection, LocalSourceMessage, LocalSourcePacket, ensure_descriptors_consumed,
    export_local_buffer, import_local_dmabuf, import_local_shm,
};

#[derive(Clone)]
pub struct LocalDestinationEndpoint {
    adapter_source: ClientSourceId,
    destination_source: ClientSourceId,
    connection: LocalPacketConnection,
}

impl HoistEndpoint for LocalDestinationEndpoint {
    fn is_available(&self) -> bool {
        !self.connection.is_disconnected()
    }

    fn has_local_receiver(&self) -> bool {
        false
    }

    fn destination(&self, source: ClientSurfaceId) -> ClientSurfaceId {
        relocated_surface(self.destination_source, source)
    }

    fn map(
        &self,
        session: HoistSessionId,
        source: ClientSurfaceId,
    ) -> ClientAdapterCommandEnvelope {
        ClientAdapterCommandEnvelope::new(
            self.adapter_source,
            HoistEndpointCommand::Map { session, source },
        )
    }

    fn unmap(&self, source: ClientSurfaceId) -> ClientAdapterCommandEnvelope {
        ClientAdapterCommandEnvelope::new(
            self.adapter_source,
            HoistEndpointCommand::Unmap { source },
        )
    }
}

pub fn local_source_registration(
    connection: LocalPacketConnection,
    upstream_source: ClientSourceId,
    adapter_source: ClientSourceId,
    destination_source: ClientSourceId,
) -> (ClientAdapterRegistration, LocalDestinationEndpoint) {
    let descriptor = ClientSourceDescriptor::new(adapter_source, ClientProvenance::Relocated);
    let adapter = LocalSourceAdapter::new(connection.clone(), upstream_source);
    (
        ClientAdapterRegistration::new(descriptor, adapter, ControlOnlyClientImporter),
        LocalDestinationEndpoint {
            adapter_source,
            destination_source,
            connection,
        },
    )
}

pub fn local_destination_registration(
    connection: LocalPacketConnection,
    upstream_source: ClientSourceId,
    destination_source: ClientSourceId,
    dmabuf: DmabufContext,
) -> ClientAdapterRegistration {
    let descriptor = ClientSourceDescriptor::new(destination_source, ClientProvenance::Relocated);
    ClientAdapterRegistration::new(
        descriptor,
        LocalDestinationAdapter::new(connection, upstream_source, destination_source, dmabuf),
        DirectClientBufferImporter,
    )
}

#[derive(Default)]
struct CachedSurface {
    role: Option<weld_client::ClientSurfaceRole>,
    commit: Option<ClientSurfaceCommit>,
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

struct LocalSourceAdapter {
    connection: LocalPacketConnection,
    upstream_source: ClientSourceId,
    cache: HashMap<ClientSurfaceId, CachedSurface>,
    mappings: HashMap<ClientSurfaceId, HoistSessionId>,
    published_buffers: HashMap<ClientBufferId, HashSet<HoistSessionId>>,
    pending_uses: HashMap<ClientBufferUseId, (HoistSessionId, ClientBufferLease)>,
    effects: Vec<ClientAdapterEffect>,
    retirements: Vec<ClientBufferUseId>,
    transport_failed: bool,
    remote_input: RemoteInputState,
}

impl LocalSourceAdapter {
    fn new(connection: LocalPacketConnection, upstream_source: ClientSourceId) -> Self {
        Self {
            connection,
            upstream_source,
            cache: HashMap::new(),
            mappings: HashMap::new(),
            published_buffers: HashMap::new(),
            pending_uses: HashMap::new(),
            effects: Vec::new(),
            retirements: Vec::new(),
            transport_failed: false,
            remote_input: RemoteInputState::default(),
        }
    }

    fn map(&mut self, session: HoistSessionId, source: ClientSurfaceId) {
        if source.source() != self.upstream_source || self.mappings.contains_key(&source) {
            return;
        }
        self.mappings.insert(source, session);
        if let Some((role, commit)) = self
            .cache
            .get(&source)
            .map(|cached| (cached.role, cached.commit.clone()))
        {
            if let Some(role) = role {
                self.send_event(
                    session,
                    ClientSurfaceEvent {
                        surface: source,
                        kind: ClientSurfaceEventKind::Role(role),
                    },
                );
            }
            if let Some(commit) = commit {
                self.send_event(
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
        self.queue_source(
            LocalSourcePacket {
                session,
                message: LocalSourceMessage::Withdraw { surface: source },
            },
            Vec::new(),
        );
        self.effects
            .push(ClientAdapterEffect::Request(ClientRequest::Surface(
                weld_client::ClientSurfaceRequest {
                    surface: source,
                    kind: ClientSurfaceRequestKind::SetPreferredScale { scale_120: None },
                },
            )));
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
        self.retire_session_buffers(session);
    }

    fn observe(&mut self, event: &ClientSurfaceEvent) {
        if event.surface.source() != self.upstream_source {
            return;
        }
        let source = event.surface;
        match &event.kind {
            ClientSurfaceEventKind::Role(role) => {
                self.cache.entry(source).or_default().role = Some(*role);
                if let weld_client::ClientSurfaceRole::Popup(popup) = role
                    && let Some(session) = self.mappings.get(&popup.owner).copied()
                {
                    self.map(session, source);
                } else if let Some(session) = self.mappings.get(&source).copied() {
                    self.send_event(session, event.clone());
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
                    self.send_event(
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
                    self.send_event(session, event.clone());
                }
            }
            ClientSurfaceEventKind::Destroyed => {
                if let Some(session) = self.mappings.remove(&source) {
                    self.send_event(session, event.clone());
                }
                self.cache.remove(&source);
            }
        }
    }

    fn send_event(&mut self, session: HoistSessionId, event: ClientSurfaceEvent) {
        if self.transport_failed || self.connection.is_disconnected() {
            return;
        }
        let mut file_descriptors = Vec::new();
        let mut added_buffer_sessions = Vec::new();
        let mut added_uses = Vec::new();
        let wire = WireClientSurfaceEvent::try_from_client(event, |lease| {
            let use_id = lease.use_id();
            let exported = export_local_buffer(
                &lease,
                file_descriptors.len(),
                self.published_buffers.contains_key(&lease.buffer()),
            )?;
            file_descriptors.extend(exported.file_descriptors);
            if matches!(
                exported.buffer.content,
                LocalBufferContent::ImportedDmabuf(_) | LocalBufferContent::ReusedDmabuf
            ) && self
                .published_buffers
                .entry(lease.buffer())
                .or_default()
                .insert(session)
            {
                added_buffer_sessions.push(lease.buffer());
            }
            self.pending_uses.insert(use_id, (session, lease));
            added_uses.push(use_id);
            Ok::<_, anyhow::Error>(exported.buffer)
        });
        match wire {
            Ok(event) => {
                if !self.queue_source(
                    LocalSourcePacket {
                        session,
                        message: LocalSourceMessage::Surface(event),
                    },
                    file_descriptors,
                ) {
                    self.rollback_export(session, &added_buffer_sessions, &added_uses);
                }
            }
            Err(error) => {
                error!(%error, ?session, "could not export a hoisted client buffer");
                self.rollback_export(session, &added_buffer_sessions, &added_uses);
                self.connection
                    .record_failure(crate::TransportError::Protocol(error.to_string()));
                self.fail_transport();
            }
        }
    }

    fn rollback_export(
        &mut self,
        session: HoistSessionId,
        buffers: &[ClientBufferId],
        uses: &[ClientBufferUseId],
    ) {
        for buffer in buffers {
            if let Some(sessions) = self.published_buffers.get_mut(buffer) {
                sessions.remove(&session);
                if sessions.is_empty() {
                    self.published_buffers.remove(buffer);
                }
            }
        }
        for use_id in uses {
            self.pending_uses.remove(use_id);
        }
    }

    fn queue_source(&mut self, packet: LocalSourcePacket, file_descriptors: Vec<OwnedFd>) -> bool {
        if self.transport_failed || self.connection.is_disconnected() {
            return false;
        }
        if let Err(error) = self.connection.queue(&packet, file_descriptors) {
            warn!(%error, "could not queue a local hoist source packet");
            self.connection.record_failure(error);
            self.fail_transport();
            return false;
        }
        true
    }

    fn receive_destination(&mut self) {
        let packets = match self.connection.drain::<LocalDestinationPacket>() {
            Ok(packets) => packets,
            Err(crate::TransportError::Disconnected) => {
                self.fail_transport();
                return;
            }
            Err(error) => {
                warn!(%error, "local hoist destination transport failed");
                self.fail_transport();
                return;
            }
        };
        for packet in packets {
            if !packet.file_descriptors.is_empty() {
                warn!("ignored descriptors attached to a destination control packet");
                continue;
            }
            let target = match &packet.message.message {
                LocalDestinationMessage::Request(request) => request_surface(request),
                LocalDestinationMessage::Input(input) => Some(input.target.surface()),
                LocalDestinationMessage::BufferReleased { .. }
                | LocalDestinationMessage::Reclaim => None,
            };
            if target.is_some_and(|surface| {
                self.mappings.get(&surface).copied() != Some(packet.message.session)
            }) {
                warn!(session = ?packet.message.session, ?target, "ignored a cross-session local hoist effect");
                continue;
            }
            match packet.message.message {
                LocalDestinationMessage::Request(request) => {
                    self.remote_input.observe_request(&request);
                    self.effects.push(ClientAdapterEffect::Request(request));
                }
                LocalDestinationMessage::Input(input) => {
                    let input = input.into_client_event();
                    self.remote_input.observe_input(&input);
                    self.effects.push(ClientAdapterEffect::Input(input));
                }
                LocalDestinationMessage::BufferReleased { use_id } => {
                    if self
                        .pending_uses
                        .get(&use_id)
                        .is_some_and(|(session, _)| *session == packet.message.session)
                    {
                        self.retirements.push(use_id);
                    } else {
                        warn!(?use_id, session = ?packet.message.session, "ignored an unknown local hoist buffer release");
                    }
                }
                LocalDestinationMessage::Reclaim => {}
            }
        }
    }

    fn clear_scale_overrides(&mut self) {
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

    fn fail_transport(&mut self) {
        if self.transport_failed {
            return;
        }
        self.transport_failed = true;
        self.effects.extend(self.remote_input.release_effects(
            self.upstream_source,
            self.mappings.keys().copied().collect(),
        ));
        self.clear_scale_overrides();
        self.pending_uses.clear();
    }

    fn retire_session_buffers(&mut self, session: HoistSessionId) {
        let retired = self
            .published_buffers
            .iter_mut()
            .filter_map(|(buffer, sessions)| {
                sessions.remove(&session);
                sessions.is_empty().then_some(*buffer)
            })
            .collect::<Vec<_>>();
        for buffer in retired {
            self.published_buffers.remove(&buffer);
            self.queue_source(
                LocalSourcePacket {
                    session,
                    message: LocalSourceMessage::BufferRetired { buffer },
                },
                Vec::new(),
            );
        }
    }

    fn retire_buffer(&mut self, buffer: ClientBufferId) {
        let Some(sessions) = self.published_buffers.remove(&buffer) else {
            return;
        };
        for session in sessions {
            self.queue_source(
                LocalSourcePacket {
                    session,
                    message: LocalSourceMessage::BufferRetired { buffer },
                },
                Vec::new(),
            );
        }
    }
}

impl ClientAdapter for LocalSourceAdapter {
    fn drain_events(&mut self, _events: &mut ClientEventQueue) {
        self.receive_destination();
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
        for use_id in self.retirements.drain(..) {
            self.pending_uses.remove(&use_id);
        }
    }

    fn observe_retired_buffer(&mut self, buffer: ClientBufferId) {
        if buffer.source() == self.upstream_source {
            self.retire_buffer(buffer);
        }
    }
}

struct ImportedDmabuf {
    local: u64,
    access: DmabufAccess,
}

struct LocalDestinationAdapter {
    connection: LocalPacketConnection,
    upstream_source: ClientSourceId,
    descriptor: ClientSourceDescriptor,
    dmabuf: DmabufContext,
    events: ClientEventQueue,
    sessions: HashMap<ClientSurfaceId, HoistSessionId>,
    buffers: HashMap<ClientBufferId, ImportedDmabuf>,
    next_buffer: Option<u64>,
    next_use: Option<u64>,
    keyboard_focus: Option<ClientSurfaceId>,
}

impl LocalDestinationAdapter {
    fn new(
        connection: LocalPacketConnection,
        upstream_source: ClientSourceId,
        destination_source: ClientSourceId,
        dmabuf: DmabufContext,
    ) -> Self {
        Self {
            connection,
            upstream_source,
            descriptor: ClientSourceDescriptor::new(
                destination_source,
                ClientProvenance::Relocated,
            ),
            dmabuf,
            events: ClientEventQueue::default(),
            sessions: HashMap::new(),
            buffers: HashMap::new(),
            next_buffer: Some(1),
            next_use: Some(1),
            keyboard_focus: None,
        }
    }

    fn receive_source(&mut self) {
        let packets = match self.connection.drain::<LocalSourcePacket>() {
            Ok(packets) => packets,
            Err(crate::TransportError::Disconnected) => {
                self.end_all_sessions();
                return;
            }
            Err(error) => {
                warn!(%error, "local hoist source transport failed");
                self.end_all_sessions();
                return;
            }
        };
        for packet in packets {
            self.apply_source_packet(packet.message, packet.file_descriptors);
        }
    }

    fn apply_source_packet(&mut self, packet: LocalSourcePacket, file_descriptors: Vec<OwnedFd>) {
        match packet.message {
            LocalSourceMessage::Surface(mut event) => {
                let source_surface = event.surface;
                let mut unreleased_uses = wire_buffer_uses(&event);
                self.sessions.insert(source_surface, packet.session);
                rewrite_wire_event(&mut event, self.descriptor.id);
                let mut descriptors = file_descriptors.into_iter().map(Some).collect::<Vec<_>>();
                let event = event.try_into_client(|buffer, metadata| {
                    let source_use = buffer.use_id;
                    let imported =
                        self.import_buffer(packet.session, buffer, metadata, &mut descriptors);
                    if imported.is_ok() {
                        unreleased_uses.retain(|use_id| *use_id != source_use);
                    }
                    imported
                });
                match event.and_then(|event| {
                    ensure_descriptors_consumed(&descriptors)?;
                    Ok(event)
                }) {
                    Ok(event) => self.events.push(event),
                    Err(error) => {
                        warn!(%error, "rejected a local hoist surface packet");
                        for use_id in unreleased_uses {
                            self.send_destination(
                                packet.session,
                                LocalDestinationMessage::BufferReleased { use_id },
                            );
                        }
                    }
                }
            }
            LocalSourceMessage::BufferRetired { buffer } => {
                if let Some(imported) = self.buffers.remove(&buffer) {
                    self.dmabuf.remove_external(&imported.access);
                }
            }
            LocalSourceMessage::Withdraw { surface } => self.destroy_surface(surface),
            LocalSourceMessage::Ended => self.end_session(packet.session),
        }
    }

    fn import_buffer(
        &mut self,
        session: HoistSessionId,
        buffer: LocalBuffer,
        metadata: ClientBufferMetadata,
        descriptors: &mut [Option<OwnedFd>],
    ) -> anyhow::Result<ClientBufferLease> {
        let LocalBuffer {
            buffer: source_buffer,
            use_id: source_use,
            content,
        } = buffer;
        enum ImportedAccess {
            Dmabuf(DmabufAccess),
            Shm(weld_core::dmabuf::WaylandShmBuffer),
        }
        let access = match content {
            LocalBufferContent::ImportedDmabuf(buffer) => {
                let external = import_local_dmabuf(buffer, metadata, descriptors)?;
                anyhow::ensure!(
                    !self.buffers.contains_key(&source_buffer),
                    "local DMA-BUF allocation was imported more than once"
                );
                ImportedAccess::Dmabuf(self.dmabuf.import_external(external)?)
            }
            LocalBufferContent::ReusedDmabuf => {
                let imported = self
                    .buffers
                    .get(&source_buffer)
                    .ok_or_else(|| anyhow::anyhow!("local DMA-BUF reuse precedes import"))?;
                ImportedAccess::Dmabuf(imported.access.clone())
            }
            LocalBufferContent::Shm(buffer) => {
                ImportedAccess::Shm(import_local_shm(buffer, metadata, descriptors)?)
            }
        };
        let local = match &access {
            ImportedAccess::Dmabuf(access) => {
                if let Some(imported) = self.buffers.get(&source_buffer) {
                    imported.local
                } else {
                    let local = self.allocate_buffer()?;
                    self.buffers.insert(
                        source_buffer,
                        ImportedDmabuf {
                            local,
                            access: access.clone(),
                        },
                    );
                    local
                }
            }
            ImportedAccess::Shm(_) => self.allocate_buffer()?,
        };
        let use_local = self.allocate_use()?;
        let connection = self.connection.clone();
        let notify = move |_| {
            let packet = LocalDestinationPacket {
                session,
                message: LocalDestinationMessage::BufferReleased { use_id: source_use },
            };
            if let Err(error) = connection.queue(&packet, Vec::new()) {
                warn!(%error, "could not release a local hoist buffer use");
            }
        };
        let buffer = ClientBufferId::new(self.descriptor.id, local);
        let use_id = ClientBufferUseId::new(self.descriptor.id, use_local);
        match access {
            ImportedAccess::Dmabuf(access) => self
                .dmabuf
                .lease_external(buffer, use_id, metadata, access, notify),
            ImportedAccess::Shm(shm) => ClientBufferLease::new(
                buffer,
                use_id,
                metadata,
                Rc::new(DirectClientBufferAccess::Shm(shm)),
                notify,
            )
            .map_err(anyhow::Error::new),
        }
    }

    fn allocate_buffer(&mut self) -> anyhow::Result<u64> {
        let current = self
            .next_buffer
            .ok_or_else(|| anyhow::anyhow!("local buffer identity space is exhausted"))?;
        self.next_buffer = current.checked_add(1);
        Ok(current)
    }

    fn allocate_use(&mut self) -> anyhow::Result<u64> {
        let current = self
            .next_use
            .ok_or_else(|| anyhow::anyhow!("local buffer-use identity space is exhausted"))?;
        self.next_use = current.checked_add(1);
        Ok(current)
    }

    fn source_surface(&self, destination: ClientSurfaceId) -> Option<ClientSurfaceId> {
        (destination.source() == self.descriptor.id).then(|| {
            ClientSurfaceId::new(
                weld_client::ClientId::new(self.upstream_source, destination.client().local()),
                destination.local(),
            )
        })
    }

    fn send_destination(&self, session: HoistSessionId, message: LocalDestinationMessage) {
        if let Err(error) = self
            .connection
            .queue(&LocalDestinationPacket { session, message }, Vec::new())
        {
            warn!(%error, "could not queue a local hoist destination packet");
        }
    }

    fn destroy_surface(&mut self, source: ClientSurfaceId) {
        self.sessions.remove(&source);
        if self
            .keyboard_focus
            .and_then(|destination| self.source_surface(destination))
            == Some(source)
        {
            self.keyboard_focus = None;
        }
        self.events.push(ClientSurfaceEvent {
            surface: relocated_surface(self.descriptor.id, source),
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
            self.destroy_surface(surface);
        }
    }

    fn end_all_sessions(&mut self) {
        let surfaces = self.sessions.keys().copied().collect::<Vec<_>>();
        for surface in surfaces {
            self.destroy_surface(surface);
        }
        for (_, imported) in self.buffers.drain() {
            self.dmabuf.remove_external(&imported.access);
        }
    }
}

impl ClientAdapter for LocalDestinationAdapter {
    fn drain_events(&mut self, events: &mut ClientEventQueue) {
        self.receive_source();
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
        self.send_destination(session, LocalDestinationMessage::Request(request));
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
        self.send_destination(session, LocalDestinationMessage::input(event));
    }

    fn apply_command(&mut self, _command: ClientAdapterCommandEnvelope) {}
    fn host_focus_lost(&mut self, _time: u32) {}
}

fn rewrite_wire_event(
    event: &mut WireClientSurfaceEvent<LocalBuffer>,
    destination: ClientSourceId,
) {
    event.surface = relocated_surface(destination, event.surface);
    match &mut event.kind {
        WireClientSurfaceEventKind::Role(weld_client::ClientSurfaceRole::Toplevel(toplevel)) => {
            toplevel.parent = toplevel
                .parent
                .map(|parent| relocated_surface(destination, parent));
        }
        WireClientSurfaceEventKind::Role(weld_client::ClientSurfaceRole::Popup(popup)) => {
            popup.owner = relocated_surface(destination, popup.owner);
        }
        WireClientSurfaceEventKind::Commit(_)
        | WireClientSurfaceEventKind::Interaction(_)
        | WireClientSurfaceEventKind::Destroyed => {}
    }
}

fn request_surface(request: &ClientRequest) -> Option<ClientSurfaceId> {
    match request {
        ClientRequest::Surface(request) => Some(request.surface),
        ClientRequest::Focus(request) => request.surface,
        ClientRequest::ClearFocus => None,
    }
}

fn wire_buffer_uses(event: &WireClientSurfaceEvent<LocalBuffer>) -> Vec<ClientBufferUseId> {
    let WireClientSurfaceEventKind::Commit(commit) = &event.kind else {
        return Vec::new();
    };
    commit
        .buffers
        .iter()
        .filter_map(|update| match &update.change {
            WireSurfaceBufferChange::Replaced { buffer, .. } => Some(buffer.use_id),
            WireSurfaceBufferChange::Retained { .. } | WireSurfaceBufferChange::Removed => None,
        })
        .collect()
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
                // Destination output identities are intentionally not forwarded.
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

#[cfg(test)]
mod tests {
    use super::*;
    use weld_client::{
        ButtonState, ClientBufferMetadata, ClientCommitRevision, ClientId, ClientSurfaceRequest,
        ClientSurfaceRole, Extent, InputPosition, LinuxButtonCode, LinuxKeycode, PointerGesture,
        RawScrollFrame, SurfaceBufferChange, SurfaceBufferUpdate, SurfaceLayerId, ToplevelState,
        TouchpadSwipe, WindowDecoration,
    };

    #[test]
    fn source_mapping_and_remote_request_keep_the_session_boundary() {
        let (source_connection, destination_connection) =
            LocalPacketConnection::pair().expect("transport pair");
        let source_id = ClientSourceId::new(0);
        let surface = ClientSurfaceId::new(ClientId::new(source_id, 1), 2);
        let session = HoistSessionId::new(3);
        let mut adapter = LocalSourceAdapter::new(source_connection.clone(), source_id);
        adapter.observe(&ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                parent: None,
                decoration: WindowDecoration::ClientSide,
            })),
        });
        adapter.apply_command(ClientAdapterCommandEnvelope::new(
            ClientSourceId::new(1),
            HoistEndpointCommand::Map {
                session,
                source: surface,
            },
        ));
        source_connection.pump().expect("source send");

        let packets = destination_connection
            .drain::<LocalSourcePacket>()
            .expect("mapped role");
        assert!(matches!(
            packets[0].message,
            LocalSourcePacket {
                session: observed,
                message: LocalSourceMessage::Surface(_),
            } if observed == session
        ));

        destination_connection
            .queue(
                &LocalDestinationPacket {
                    session,
                    message: LocalDestinationMessage::Request(ClientRequest::Surface(
                        ClientSurfaceRequest {
                            surface,
                            kind: ClientSurfaceRequestKind::Configure {
                                logical_size: Extent::new(800, 600),
                                resizing: true,
                            },
                        },
                    )),
                },
                Vec::new(),
            )
            .expect("destination request");
        destination_connection.pump().expect("destination send");
        adapter.drain_events(&mut ClientEventQueue::default());
        let mut effects = Vec::new();
        adapter.drain_effects(&mut effects);

        assert!(matches!(
            effects.as_slice(),
            [ClientAdapterEffect::Request(ClientRequest::Surface(request))]
                if request.surface == surface
                    && request.kind == (ClientSurfaceRequestKind::Configure {
                        logical_size: Extent::new(800, 600),
                        resizing: true,
                    })
        ));
    }

    #[test]
    fn source_transports_shm_without_publishing_a_reusable_allocation() {
        let (source_connection, destination_connection) =
            LocalPacketConnection::pair().expect("transport pair");
        let source = ClientSourceId::new(0);
        let session = HoistSessionId::new(3);
        let surface = ClientSurfaceId::new(ClientId::new(source, 1), 2);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), false);
        let lease = ClientBufferLease::new(
            ClientBufferId::new(source, 4),
            ClientBufferUseId::new(source, 5),
            metadata,
            Rc::new(DirectClientBufferAccess::Shm(
                weld_core::dmabuf::WaylandShmBuffer {
                    bgra_pixels: vec![1, 2, 3, 4],
                },
            )),
            |_| {},
        )
        .expect("matching source");
        let mut adapter = LocalSourceAdapter::new(source_connection.clone(), source);

        adapter.send_event(
            session,
            ClientSurfaceEvent {
                surface,
                kind: ClientSurfaceEventKind::Commit(ClientSurfaceCommit {
                    revision: ClientCommitRevision::new(1),
                    mapped: true,
                    root: None,
                    window_geometry: None,
                    overlays: Vec::new(),
                    inputs: Vec::new(),
                    buffers: vec![SurfaceBufferUpdate {
                        layer: SurfaceLayerId::new(1),
                        change: SurfaceBufferChange::Replaced {
                            metadata,
                            buffer: lease,
                        },
                    }],
                }),
            },
        );
        source_connection.pump().expect("source send");
        let packets = destination_connection
            .drain::<LocalSourcePacket>()
            .expect("SHM packet");

        assert!(adapter.published_buffers.is_empty());
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].file_descriptors.len(), 1);
        let LocalSourceMessage::Surface(event) = &packets[0].message.message else {
            panic!("expected surface packet");
        };
        let WireClientSurfaceEventKind::Commit(commit) = &event.kind else {
            panic!("expected surface commit");
        };
        assert!(matches!(
            &commit.buffers[0].change,
            WireSurfaceBufferChange::Replaced {
                buffer: LocalBuffer {
                    content: LocalBufferContent::Shm(_),
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn disconnect_releases_remote_input_and_focus_exactly() {
        let source = ClientSourceId::new(0);
        let surface = ClientSurfaceId::new(ClientId::new(source, 1), 2);
        let pointer = ClientInputTarget::Pointer {
            surface,
            layer: weld_client::SurfaceLayerId::new(1),
        };
        let keyboard = ClientInputTarget::Keyboard { surface };
        let mut state = RemoteInputState::default();
        state.observe_request(&ClientRequest::Focus(weld_client::ClientFocusRequest {
            source,
            surface: Some(surface),
        }));
        for event in [
            ClientInputEvent {
                target: pointer,
                host_position: None,
                event: InputEventKind::PointerButton {
                    position: Some(InputPosition::new(3.0, 4.0)),
                    button: LinuxButtonCode(1),
                    state: ButtonState::Pressed,
                },
                time: 5,
            },
            ClientInputEvent {
                target: keyboard,
                host_position: None,
                event: InputEventKind::Keyboard {
                    keycode: LinuxKeycode(6),
                    state: ButtonState::Pressed,
                },
                time: 5,
            },
            ClientInputEvent {
                target: pointer,
                host_position: None,
                event: InputEventKind::PointerGesture {
                    gesture: PointerGesture::Swipe(TouchpadSwipe::Begin { fingers: 3 }),
                },
                time: 5,
            },
            ClientInputEvent {
                target: pointer,
                host_position: None,
                event: InputEventKind::PointerAxis {
                    position: None,
                    axis: RawScrollFrame {
                        source: RawScrollSource::Finger,
                        phase: RawScrollPhase::Started,
                        horizontal: 1.0,
                        vertical: 1.0,
                        horizontal_v120: None,
                        vertical_v120: None,
                        horizontal_stop: false,
                        vertical_stop: false,
                    },
                },
                time: 5,
            },
        ] {
            state.observe_input(&event);
        }

        let effects = state.release_effects(source, HashSet::from([surface]));

        assert!(effects.iter().any(|effect| matches!(
            effect,
            ClientAdapterEffect::Input(ClientInputEvent {
                event: InputEventKind::PointerButton {
                    state: ButtonState::Released,
                    ..
                },
                ..
            })
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            ClientAdapterEffect::Input(ClientInputEvent {
                event: InputEventKind::Keyboard {
                    state: ButtonState::Released,
                    ..
                },
                ..
            })
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            ClientAdapterEffect::Input(ClientInputEvent {
                event: InputEventKind::PointerGesture { gesture },
                ..
            }) if gesture.is_end()
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            ClientAdapterEffect::Input(ClientInputEvent {
                event: InputEventKind::PointerAxis { axis, .. },
                ..
            }) if axis.phase == RawScrollPhase::Cancelled
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            ClientAdapterEffect::Request(ClientRequest::Focus(focus))
                if focus.source == source && focus.surface.is_none()
        )));
    }

    #[test]
    fn retired_source_buffer_is_forwarded_once() {
        let (source_connection, destination_connection) =
            LocalPacketConnection::pair().expect("transport pair");
        let source_id = ClientSourceId::new(0);
        let session = HoistSessionId::new(1);
        let buffer = ClientBufferId::new(source_id, 2);
        let mut adapter = LocalSourceAdapter::new(source_connection.clone(), source_id);
        adapter
            .published_buffers
            .insert(buffer, HashSet::from([session]));

        adapter.observe_retired_buffer(buffer);
        source_connection.pump().expect("source send");
        let packets = destination_connection
            .drain::<LocalSourcePacket>()
            .expect("retirement packet");

        assert!(matches!(
            packets[0].message.message,
            LocalSourceMessage::BufferRetired { buffer: retired } if retired == buffer
        ));
        assert!(!adapter.published_buffers.contains_key(&buffer));
    }
}
