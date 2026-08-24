//! Runtime-independent hoist sessions and same-process loopback client adapter.

use std::collections::{HashMap, VecDeque};

use weld_client::{
    ClientAdapter, ClientAdapterCommandEnvelope, ClientAdapterRegistration, ClientBufferId,
    ClientBufferUseId, ClientEventQueue, ClientInputEvent, ClientProvenance, ClientRequest,
    ClientRouteAliasUpdate, ClientSourceDescriptor, ClientSourceId, ClientSurfaceCommit,
    ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceId, ClientSurfaceRole,
    PassthroughClientImporter, PopupState, SurfaceBufferChange, ToplevelState,
};

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HoistSessionId(u64);

impl HoistSessionId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HoistFamilyId(u64);

impl HoistFamilyId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoistSessionPhase {
    Active,
    Closed,
    Ending,
    Reclaiming,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum HoistSourceMode {
    PreservedSlot,
    Followed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReclaimScope {
    Member,
    Family,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoistEndpointCommand {
    Map { source: ClientSurfaceId },
    Unmap { source: ClientSurfaceId },
}

#[derive(Clone, Copy, Debug)]
pub struct LoopbackEndpoint {
    source: ClientSourceId,
}

impl LoopbackEndpoint {
    pub const fn source(self) -> ClientSourceId {
        self.source
    }

    pub fn map(self, source: ClientSurfaceId) -> ClientAdapterCommandEnvelope {
        ClientAdapterCommandEnvelope::new(self.source, HoistEndpointCommand::Map { source })
    }

    pub fn unmap(self, source: ClientSurfaceId) -> ClientAdapterCommandEnvelope {
        ClientAdapterCommandEnvelope::new(self.source, HoistEndpointCommand::Unmap { source })
    }

    pub const fn destination(self, source: ClientSurfaceId) -> ClientSurfaceId {
        relocated_surface(self.source, source)
    }
}

pub fn loopback_registration(
    upstream: ClientSourceId,
    destination: ClientSourceId,
) -> (ClientAdapterRegistration, LoopbackEndpoint) {
    let descriptor = ClientSourceDescriptor::new(destination, ClientProvenance::Relocated);
    let adapter = LoopbackClientAdapter::new(upstream, descriptor);
    (
        ClientAdapterRegistration::new(descriptor, adapter, PassthroughClientImporter),
        LoopbackEndpoint {
            source: destination,
        },
    )
}

pub const fn relocated_surface(
    destination: ClientSourceId,
    source: ClientSurfaceId,
) -> ClientSurfaceId {
    ClientSurfaceId::new(
        weld_client::ClientId::new(destination, source.client().local()),
        source.local(),
    )
}

#[derive(Default)]
struct CachedSurface {
    role: Option<ClientSurfaceRole>,
    commit: Option<ClientSurfaceCommit>,
}

struct LoopbackClientAdapter {
    upstream: ClientSourceId,
    descriptor: ClientSourceDescriptor,
    cache: HashMap<ClientSurfaceId, CachedSurface>,
    mappings: HashMap<ClientSurfaceId, ClientSurfaceId>,
    events: ClientEventQueue,
    aliases: VecDeque<ClientRouteAliasUpdate>,
    next_buffer_use: Option<u64>,
}

impl LoopbackClientAdapter {
    fn new(upstream: ClientSourceId, descriptor: ClientSourceDescriptor) -> Self {
        Self {
            upstream,
            descriptor,
            cache: HashMap::new(),
            mappings: HashMap::new(),
            events: ClientEventQueue::default(),
            aliases: VecDeque::new(),
            next_buffer_use: Some(1),
        }
    }

    fn map(&mut self, source: ClientSurfaceId) {
        if source.source() != self.upstream || self.mappings.contains_key(&source) {
            return;
        }
        let destination = relocated_surface(self.descriptor.id, source);
        self.mappings.insert(source, destination);
        self.aliases.push_back(ClientRouteAliasUpdate {
            destination,
            source: Some(source),
        });
        if let Some(cached) = self.cache.get(&source) {
            if let Some(role) = cached.role.and_then(|role| self.relay_role(role)) {
                self.events.push(ClientSurfaceEvent {
                    surface: destination,
                    kind: ClientSurfaceEventKind::Role(role),
                });
            }
            if let Some(commit) = cached.commit.clone() {
                self.push_commit(source, commit);
            }
        }
        self.map_cached_popups(source);
        self.refresh_children_of(source);
    }

    fn unmap(&mut self, source: ClientSurfaceId) {
        let Some(destination) = self.mappings.remove(&source) else {
            return;
        };
        self.aliases.push_back(ClientRouteAliasUpdate {
            destination,
            source: None,
        });
        self.events.push(ClientSurfaceEvent {
            surface: destination,
            kind: ClientSurfaceEventKind::Destroyed,
        });
        let popups = self
            .cache
            .iter()
            .filter_map(|(surface, cached)| match cached.role {
                Some(ClientSurfaceRole::Popup(popup)) if popup.owner == source => Some(*surface),
                _ => None,
            })
            .collect::<Vec<_>>();
        for popup in popups {
            self.unmap(popup);
        }
    }

    fn relay_role(&self, role: ClientSurfaceRole) -> Option<ClientSurfaceRole> {
        match role {
            ClientSurfaceRole::Toplevel(toplevel) => {
                Some(ClientSurfaceRole::Toplevel(ToplevelState {
                    parent: toplevel
                        .parent
                        .and_then(|parent| self.mappings.get(&parent).copied()),
                    decoration: toplevel.decoration,
                }))
            }
            ClientSurfaceRole::Popup(popup) => {
                let owner = self.mappings.get(&popup.owner).copied()?;
                Some(ClientSurfaceRole::Popup(PopupState { owner, ..popup }))
            }
        }
    }

    fn push_commit(&mut self, source: ClientSurfaceId, mut commit: ClientSurfaceCommit) {
        let Some(destination) = self.mappings.get(&source).copied() else {
            return;
        };
        for update in &mut commit.buffers {
            let SurfaceBufferChange::Replaced { metadata, buffer } = &mut update.change else {
                continue;
            };
            let Some(use_local) = self.next_buffer_use else {
                update.change = SurfaceBufferChange::Retained {
                    metadata: *metadata,
                };
                continue;
            };
            self.next_buffer_use = use_local.checked_add(1);
            let destination_buffer =
                ClientBufferId::new(self.descriptor.id, buffer.buffer().local());
            let destination_use = ClientBufferUseId::new(self.descriptor.id, use_local);
            match buffer.clone().relay(destination_buffer, destination_use) {
                Ok(relayed) => *buffer = relayed,
                Err(_) => {
                    update.change = SurfaceBufferChange::Retained {
                        metadata: *metadata,
                    };
                }
            }
        }
        self.events.push(ClientSurfaceEvent {
            surface: destination,
            kind: ClientSurfaceEventKind::Commit(commit),
        });
    }

    fn map_cached_popups(&mut self, owner: ClientSurfaceId) {
        let popups = self
            .cache
            .iter()
            .filter_map(|(surface, cached)| match cached.role {
                Some(ClientSurfaceRole::Popup(popup)) if popup.owner == owner => Some(*surface),
                _ => None,
            })
            .collect::<Vec<_>>();
        for popup in popups {
            self.map(popup);
        }
    }

    fn refresh_children_of(&mut self, parent: ClientSurfaceId) {
        let children = self
            .cache
            .iter()
            .filter_map(|(surface, cached)| match cached.role {
                Some(ClientSurfaceRole::Toplevel(ToplevelState {
                    parent: Some(candidate),
                    ..
                })) if candidate == parent && self.mappings.contains_key(surface) => Some(*surface),
                _ => None,
            })
            .collect::<Vec<_>>();
        for child in children {
            if let Some(destination) = self.mappings.get(&child).copied()
                && let Some(role) = self.cache[&child]
                    .role
                    .and_then(|role| self.relay_role(role))
            {
                self.events.push(ClientSurfaceEvent {
                    surface: destination,
                    kind: ClientSurfaceEventKind::Role(role),
                });
            }
        }
    }

    fn observe(&mut self, event: &ClientSurfaceEvent) {
        if event.surface.source() != self.upstream {
            return;
        }
        let source = event.surface;
        match &event.kind {
            ClientSurfaceEventKind::Role(role) => {
                self.cache.entry(source).or_default().role = Some(*role);
                let auto_popup = matches!(
                    role,
                    ClientSurfaceRole::Popup(popup) if self.mappings.contains_key(&popup.owner)
                );
                if auto_popup && !self.mappings.contains_key(&source) {
                    self.map(source);
                } else if let Some(destination) = self.mappings.get(&source).copied()
                    && let Some(role) = self.relay_role(*role)
                {
                    self.events.push(ClientSurfaceEvent {
                        surface: destination,
                        kind: ClientSurfaceEventKind::Role(role),
                    });
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
                self.push_commit(source, outgoing);
            }
            ClientSurfaceEventKind::Interaction(interaction) => {
                if let Some(destination) = self.mappings.get(&source).copied() {
                    self.events.push(ClientSurfaceEvent {
                        surface: destination,
                        kind: ClientSurfaceEventKind::Interaction(*interaction),
                    });
                }
            }
            ClientSurfaceEventKind::Destroyed => {
                self.unmap(source);
                self.cache.remove(&source);
            }
        }
    }
}

impl ClientAdapter for LoopbackClientAdapter {
    fn drain_events(&mut self, events: &mut ClientEventQueue) {
        while let Some(event) = self.events.pop_front() {
            events.push(event);
        }
    }

    fn apply_request(&mut self, _request: ClientRequest) {}
    fn apply_input(&mut self, _event: ClientInputEvent) {}

    fn apply_command(&mut self, command: ClientAdapterCommandEnvelope) {
        let Ok(command) = command.downcast::<HoistEndpointCommand>() else {
            return;
        };
        match *command {
            HoistEndpointCommand::Map { source } => self.map(source),
            HoistEndpointCommand::Unmap { source } => self.unmap(source),
        }
    }

    fn host_focus_lost(&mut self, _time: u32) {}

    fn observe_event(&mut self, event: &ClientSurfaceEvent) {
        self.observe(event);
    }

    fn drain_route_alias_updates(&mut self, updates: &mut Vec<ClientRouteAliasUpdate>) {
        updates.extend(self.aliases.drain(..));
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use weld_client::{
        ButtonState, ClientAdapter, ClientAdapterCommandEnvelope, ClientBufferId,
        ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, ClientCommitRevision,
        ClientEventQueue, ClientFocusRequest, ClientInputEvent, ClientProvenance, ClientRequest,
        ClientRuntime, ClientRuntimeAdapter, ClientSourceDescriptor, ClientSourceId,
        ClientSurfaceCommit, ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceRequest,
        ClientSurfaceRequestKind, InputEventKind, LinuxKeycode, SurfaceBufferChange,
        SurfaceBufferUpdate, ToplevelInteractionRequestKind, ToplevelState, WindowDecoration,
    };

    use super::*;

    #[derive(Default)]
    struct UpstreamRecord {
        events: ClientEventQueue,
        requests: Vec<ClientRequest>,
        inputs: Vec<ClientInputEvent>,
    }

    struct UpstreamAdapter(Rc<RefCell<UpstreamRecord>>);

    impl ClientAdapter for UpstreamAdapter {
        fn drain_events(&mut self, events: &mut ClientEventQueue) {
            while let Some(event) = self.0.borrow_mut().events.pop_front() {
                events.push(event);
            }
        }

        fn apply_request(&mut self, request: ClientRequest) {
            self.0.borrow_mut().requests.push(request);
        }

        fn apply_input(&mut self, event: ClientInputEvent) {
            self.0.borrow_mut().inputs.push(event);
        }

        fn apply_command(&mut self, _command: ClientAdapterCommandEnvelope) {}
        fn host_focus_lost(&mut self, _time: u32) {}
    }

    fn source(source: ClientSourceId, local: u64) -> ClientSurfaceId {
        ClientSurfaceId::new(weld_client::ClientId::new(source, 1), local)
    }

    fn runtime() -> (ClientRuntime, Rc<RefCell<UpstreamRecord>>, LoopbackEndpoint) {
        let upstream = ClientSourceId::new(0);
        let destination = ClientSourceId::new(1);
        let record = Rc::new(RefCell::new(UpstreamRecord::default()));
        let mut runtime = ClientRuntime::default();
        runtime
            .register(ClientRuntimeAdapter::new(
                ClientSourceDescriptor::new(upstream, ClientProvenance::Local),
                UpstreamAdapter(record.clone()),
            ))
            .expect("unique upstream");
        let (registration, endpoint) = loopback_registration(upstream, destination);
        runtime
            .register(registration.into_parts().runtime)
            .expect("unique loopback");
        (runtime, record, endpoint)
    }

    #[test]
    fn map_replays_in_the_same_drain_and_rewrites_surface_requests() {
        let (mut runtime, upstream, endpoint) = runtime();
        let source = source(ClientSourceId::new(0), 7);
        upstream.borrow_mut().events.push(ClientSurfaceEvent {
            surface: source,
            kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                parent: None,
                decoration: WindowDecoration::ServerSide,
            })),
        });
        assert!(runtime.apply_command(endpoint.map(source)));
        let mut events = ClientEventQueue::default();
        let mut invalid = Vec::new();

        runtime.drain_events(&mut events, &mut invalid);

        assert!(invalid.is_empty());
        let observed = [events.pop_front(), events.pop_front()];
        assert_eq!(
            observed
                .iter()
                .flatten()
                .map(|event| event.surface)
                .collect::<Vec<_>>(),
            vec![source, endpoint.destination(source)]
        );
        assert!(
            runtime.apply_request(ClientRequest::Surface(ClientSurfaceRequest {
                surface: endpoint.destination(source),
                kind: ClientSurfaceRequestKind::Close,
            }))
        );
        assert_eq!(
            upstream.borrow().requests.last(),
            Some(&ClientRequest::Surface(ClientSurfaceRequest {
                surface: source,
                kind: ClientSurfaceRequestKind::Close,
            }))
        );
    }

    #[test]
    fn mapped_owner_auto_maps_popups_and_unmap_retires_the_alias() {
        let (mut runtime, upstream, endpoint) = runtime();
        let owner = source(ClientSourceId::new(0), 1);
        let popup = source(ClientSourceId::new(0), 2);
        assert!(runtime.apply_command(endpoint.map(owner)));
        upstream.borrow_mut().events.push(ClientSurfaceEvent {
            surface: owner,
            kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                parent: None,
                decoration: WindowDecoration::ClientSide,
            })),
        });
        upstream.borrow_mut().events.push(ClientSurfaceEvent {
            surface: popup,
            kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(PopupState {
                owner,
                position: weld_client::LogicalPoint::new(2.0, 3.0),
                stack_index: 1,
            })),
        });
        let mut events = ClientEventQueue::default();
        let mut invalid = Vec::new();
        runtime.drain_events(&mut events, &mut invalid);
        let mut surfaces = Vec::new();
        while let Some(event) = events.pop_front() {
            surfaces.push(event.surface);
        }
        assert!(surfaces.contains(&endpoint.destination(popup)));

        assert!(runtime.apply_command(endpoint.unmap(owner)));
        runtime.drain_events(&mut events, &mut invalid);
        assert!(
            !runtime.apply_request(ClientRequest::Surface(ClientSurfaceRequest {
                surface: endpoint.destination(owner),
                kind: ClientSurfaceRequestKind::Close,
            }))
        );
    }

    #[test]
    fn mapped_toplevel_relays_client_interactions_to_the_destination_identity() {
        let (mut runtime, upstream, endpoint) = runtime();
        let source = source(ClientSourceId::new(0), 3);
        assert!(runtime.apply_command(endpoint.map(source)));
        upstream.borrow_mut().events.push(ClientSurfaceEvent {
            surface: source,
            kind: ClientSurfaceEventKind::Interaction(ToplevelInteractionRequestKind::Move),
        });
        let mut events = ClientEventQueue::default();
        let mut invalid = Vec::new();

        runtime.drain_events(&mut events, &mut invalid);

        assert!(invalid.is_empty());
        let mut relayed = false;
        while let Some(event) = events.pop_front() {
            relayed |= event.surface == endpoint.destination(source)
                && matches!(
                    event.kind,
                    ClientSurfaceEventKind::Interaction(ToplevelInteractionRequestKind::Move)
                );
        }
        assert!(relayed);
    }

    #[test]
    fn relayed_buffer_completes_upstream_only_after_cache_and_destination_consumers() {
        let completed = Rc::new(RefCell::new(Vec::new()));
        let completed_for_callback = completed.clone();
        let upstream_source = ClientSourceId::new(0);
        let destination_source = ClientSourceId::new(1);
        let upstream = ClientBufferLease::new(
            ClientBufferId::new(upstream_source, 3),
            ClientBufferUseId::new(upstream_source, 4),
            ClientBufferMetadata::new(weld_client::Extent::new(1, 1), false),
            Rc::new(String::from("native access")),
            move |use_id| completed_for_callback.borrow_mut().push(use_id),
        )
        .expect("matching upstream source");
        let cache = upstream.clone();
        let destination = upstream
            .relay(
                ClientBufferId::new(destination_source, 3),
                ClientBufferUseId::new(destination_source, 1),
            )
            .expect("matching destination source");
        let renderer = destination.clone();

        drop(destination);
        drop(renderer);
        assert!(completed.borrow().is_empty());
        drop(cache);
        assert_eq!(
            completed.borrow().as_slice(),
            [ClientBufferUseId::new(upstream_source, 4)]
        );
    }

    #[test]
    fn cached_replacement_survives_retained_commits_for_later_map() {
        let (mut runtime, upstream, endpoint) = runtime();
        let surface = source(ClientSourceId::new(0), 9);
        let metadata = ClientBufferMetadata::new(weld_client::Extent::new(1, 1), false);
        let lease = ClientBufferLease::new(
            ClientBufferId::new(ClientSourceId::new(0), 1),
            ClientBufferUseId::new(ClientSourceId::new(0), 1),
            metadata,
            Rc::new(()),
            |_| {},
        )
        .expect("matching source");
        let commit = |revision, change| ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Commit(ClientSurfaceCommit {
                revision: ClientCommitRevision::new(revision),
                mapped: true,
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: vec![SurfaceBufferUpdate {
                    layer: weld_client::SurfaceLayerId::new(1),
                    change,
                }],
            }),
        };
        upstream.borrow_mut().events.push(commit(
            1,
            SurfaceBufferChange::Replaced {
                metadata,
                buffer: lease,
            },
        ));
        let mut events = ClientEventQueue::default();
        let mut invalid = Vec::new();
        runtime.drain_events(&mut events, &mut invalid);
        upstream
            .borrow_mut()
            .events
            .push(commit(2, SurfaceBufferChange::Retained { metadata }));
        runtime.drain_events(&mut events, &mut invalid);
        assert!(runtime.apply_command(endpoint.map(surface)));
        runtime.drain_events(&mut events, &mut invalid);

        let mut relayed_replacement = false;
        while let Some(event) = events.pop_front() {
            if event.surface == endpoint.destination(surface)
                && let ClientSurfaceEventKind::Commit(commit) = event.kind
            {
                relayed_replacement |= matches!(
                    commit.buffers[0].change,
                    SurfaceBufferChange::Replaced { .. }
                );
            }
        }
        assert!(relayed_replacement);
    }

    #[test]
    fn focus_alias_rewrites_surface_and_source() {
        let (mut runtime, upstream, endpoint) = runtime();
        let surface = source(ClientSourceId::new(0), 5);
        assert!(runtime.apply_command(endpoint.map(surface)));
        let mut events = ClientEventQueue::default();
        let mut invalid = Vec::new();
        runtime.drain_events(&mut events, &mut invalid);
        assert!(
            runtime.apply_request(ClientRequest::Focus(ClientFocusRequest {
                source: endpoint.source(),
                surface: Some(endpoint.destination(surface)),
            }))
        );
        assert_eq!(
            upstream.borrow().requests.last(),
            Some(&ClientRequest::Focus(ClientFocusRequest {
                source: ClientSourceId::new(0),
                surface: Some(surface),
            }))
        );
        assert_eq!(
            runtime.dispatch_unconsumed_input(weld_client::RuntimeInputEvent::new(
                weld_client::RuntimeInputEventKind::Input(InputEventKind::Keyboard {
                    keycode: LinuxKeycode(1),
                    state: ButtonState::Pressed,
                }),
                1,
            )),
            weld_client::ClientInputDispatchResult::Delivered
        );
    }
}
