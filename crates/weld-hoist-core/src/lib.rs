//! Runtime-independent hoist sessions and same-process loopback client adapter.

mod loopback;
mod relay;

pub use relay::{
    DestinationPortCommand, DestinationPortEvent, DestinationPortRecord, DestinationRelayAdapter,
    HoistDestinationPort, HoistPortError, HoistPortResult, HoistSourcePort, SourcePortCommand,
    SourceRelayAdapter,
};
use weld_client::{
    ClientAdapterCommandEnvelope, ClientAdapterRegistration, ClientProvenance,
    ClientSourceDescriptor, ClientSourceId, ClientSurfaceId,
};
pub use weld_hoist_protocol::HoistSessionId;

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
    Map {
        session: HoistSessionId,
        source: ClientSurfaceId,
    },
    Unmap {
        source: ClientSurfaceId,
    },
}

pub trait HoistEndpoint: Send + Sync {
    fn is_available(&self) -> bool {
        true
    }

    fn has_local_receiver(&self) -> bool {
        true
    }

    fn destination(&self, source: ClientSurfaceId) -> ClientSurfaceId;
    fn map(&self, session: HoistSessionId, source: ClientSurfaceId)
    -> ClientAdapterCommandEnvelope;
    fn unmap(&self, source: ClientSurfaceId) -> ClientAdapterCommandEnvelope;
}

#[derive(Clone, Copy, Debug)]
pub struct LoopbackEndpoint {
    source: ClientSourceId,
}

impl LoopbackEndpoint {
    pub const fn source(self) -> ClientSourceId {
        self.source
    }

    pub fn map(
        self,
        session: HoistSessionId,
        source: ClientSurfaceId,
    ) -> ClientAdapterCommandEnvelope {
        ClientAdapterCommandEnvelope::new(
            self.source,
            HoistEndpointCommand::Map { session, source },
        )
    }

    pub fn unmap(self, source: ClientSurfaceId) -> ClientAdapterCommandEnvelope {
        ClientAdapterCommandEnvelope::new(self.source, HoistEndpointCommand::Unmap { source })
    }

    pub const fn destination(self, source: ClientSurfaceId) -> ClientSurfaceId {
        relocated_surface(self.source, source)
    }
}

impl HoistEndpoint for LoopbackEndpoint {
    fn destination(&self, source: ClientSurfaceId) -> ClientSurfaceId {
        (*self).destination(source)
    }

    fn map(
        &self,
        session: HoistSessionId,
        source: ClientSurfaceId,
    ) -> ClientAdapterCommandEnvelope {
        (*self).map(session, source)
    }

    fn unmap(&self, source: ClientSurfaceId) -> ClientAdapterCommandEnvelope {
        (*self).unmap(source)
    }
}

pub fn loopback_registration(
    upstream: ClientSourceId,
    destination: ClientSourceId,
) -> (ClientAdapterRegistration, LoopbackEndpoint) {
    let descriptor = ClientSourceDescriptor::new(destination, ClientProvenance::Relocated);
    (
        loopback::registration(upstream, descriptor),
        loopback::endpoint(destination),
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

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use weld_client::{
        ButtonState, ClientAdapter, ClientAdapterCommandEnvelope, ClientBufferId,
        ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, ClientCommitRevision,
        ClientEventQueue, ClientFocusRequest, ClientInputEvent, ClientProvenance, ClientRequest,
        ClientRuntime, ClientRuntimeAdapter, ClientSourceDescriptor, ClientSourceId,
        ClientSurfaceCommit, ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceRequest,
        ClientSurfaceRequestKind, ClientSurfaceRole, InputEventKind, LinuxKeycode, PopupState,
        SurfaceBufferChange, SurfaceBufferUpdate, ToplevelInteractionRequestKind, ToplevelState,
        WindowDecoration,
    };

    use super::*;

    #[derive(Default)]
    struct UpstreamRecord {
        events: ClientEventQueue,
        requests: Vec<ClientRequest>,
        inputs: Vec<ClientInputEvent>,
        cursors: Vec<weld_client::ClientCursorUpdate>,
    }

    struct UpstreamAdapter(Rc<RefCell<UpstreamRecord>>);

    impl ClientAdapter for UpstreamAdapter {
        fn drain_cursor_updates(&mut self, updates: &mut Vec<weld_client::ClientCursorUpdate>) {
            updates.append(&mut self.0.borrow_mut().cursors);
        }
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
    fn loopback_cursor_feedback_is_available_in_the_same_runtime_drain() {
        let (mut runtime, upstream, endpoint) = runtime();
        let surface = source(ClientSourceId::new(0), 7);
        upstream.borrow_mut().events.push(ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Commit(ClientSurfaceCommit {
                revision: ClientCommitRevision::new(1),
                alpha_mode: Default::default(),
                mapped: true,
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: Vec::new(),
            }),
        });
        upstream.borrow_mut().events.push(ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                parent: None,
                decoration: WindowDecoration::ClientSide,
            })),
        });
        runtime.drain_events(&mut ClientEventQueue::default(), &mut Vec::new());
        runtime.apply_command(endpoint.map(HoistSessionId::new(1), surface));
        upstream
            .borrow_mut()
            .cursors
            .push(weld_client::ClientCursorUpdate {
                surface,
                cursor: weld_client::ClientCursor::Named(weld_client::CursorIcon::Text),
            });
        runtime.drain_events(&mut ClientEventQueue::default(), &mut Vec::new());
        runtime.set_pointer_route(Some(weld_client::ClientPointerRoute {
            surface: endpoint.destination(surface),
            layer: weld_client::SurfaceLayerId::new(1),
            transform: weld_client::InputTransform::IDENTITY,
        }));
        assert_eq!(
            runtime.pointer_cursor(),
            Some((
                surface,
                weld_client::ClientCursor::Named(weld_client::CursorIcon::Text)
            ))
        );
        runtime.apply_command(endpoint.unmap(surface));
        runtime.drain_events(&mut ClientEventQueue::default(), &mut Vec::new());
        assert_eq!(runtime.pointer_cursor(), None);
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
        assert!(runtime.apply_command(endpoint.map(HoistSessionId::new(1), source)));
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
        assert!(runtime.apply_command(endpoint.map(HoistSessionId::new(1), owner)));
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
    fn mapped_popup_relays_each_position_and_stack_update_once() {
        let (mut runtime, upstream, endpoint) = runtime();
        let owner = source(ClientSourceId::new(0), 1);
        let popup = source(ClientSourceId::new(0), 2);
        assert!(runtime.apply_command(endpoint.map(HoistSessionId::new(1), owner)));
        let mut events = ClientEventQueue::default();
        let mut invalid = Vec::new();
        for (x, y, stack_index) in [(0.0, 0.0, 1), (400.0, 32.0, 2), (-20.0, 250.0, 3)] {
            let position = weld_client::LogicalPoint::new(x, y);
            upstream.borrow_mut().events.push(ClientSurfaceEvent {
                surface: popup,
                kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(PopupState {
                    owner,
                    position,
                    stack_index,
                })),
            });
            runtime.drain_events(&mut events, &mut invalid);
            let mut received = Vec::new();
            while let Some(event) = events.pop_front() {
                if event.surface == endpoint.destination(popup)
                    && let ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(role)) = event.kind
                {
                    received.push(role);
                }
            }
            assert_eq!(
                received,
                vec![PopupState {
                    owner: endpoint.destination(owner),
                    position,
                    stack_index,
                }]
            );
            assert!(invalid.is_empty());
        }
    }

    #[test]
    fn mapped_toplevel_relays_client_interactions_to_the_destination_identity() {
        let (mut runtime, upstream, endpoint) = runtime();
        let source = source(ClientSourceId::new(0), 3);
        assert!(runtime.apply_command(endpoint.map(HoistSessionId::new(1), source)));
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
                alpha_mode: Default::default(),
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
        assert!(runtime.apply_command(endpoint.map(HoistSessionId::new(1), surface)));
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
        assert!(runtime.apply_command(endpoint.map(HoistSessionId::new(1), surface)));
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

    #[test]
    fn duplicate_map_and_unknown_unmap_do_not_retire_the_live_alias() {
        let (mut runtime, upstream, endpoint) = runtime();
        let surface = source(ClientSourceId::new(0), 11);
        let unknown = source(ClientSourceId::new(0), 12);
        let session = HoistSessionId::new(1);
        assert!(runtime.apply_command(endpoint.map(session, surface)));
        let mut events = ClientEventQueue::default();
        let mut invalid = Vec::new();
        runtime.drain_events(&mut events, &mut invalid);

        assert!(runtime.apply_command(endpoint.map(session, surface)));
        assert!(runtime.apply_command(endpoint.unmap(unknown)));
        runtime.drain_events(&mut events, &mut invalid);

        assert!(
            runtime.apply_request(ClientRequest::Surface(ClientSurfaceRequest {
                surface: endpoint.destination(surface),
                kind: ClientSurfaceRequestKind::Close,
            }))
        );
        assert_eq!(
            upstream.borrow().requests.last(),
            Some(&ClientRequest::Surface(ClientSurfaceRequest {
                surface,
                kind: ClientSurfaceRequestKind::Close,
            }))
        );
    }
}
