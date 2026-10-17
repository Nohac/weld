//! Codec-backed source relays for admitted client sessions.
use crate::SourceConnection;
use crate::{EncodeBackend, EncodedSourceOptions, EncodedSourcePort, SharedBitrateBudget};
use anyhow::Result;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Instant,
};
use weld_client::{
    ClientAdapter, ClientAdapterCommandEnvelope, ClientAdapterEffect, ClientAdapterRegistration,
    ClientBufferId, ClientCursorUpdate, ClientEventQueue, ClientInputEvent,
    ClientPresentationUpdate, ClientProvenance, ClientRequest, ClientSourceDescriptor,
    ClientSourceId, ClientSurfaceEvent, ClientSurfaceId, ControlOnlyClientImporter,
};
use weld_hoist_core::access::{AuthorizedEndpoint, authorize_endpoint};
use weld_hoist_core::session::SessionId;
use weld_hoist_core::session::admission::SourceAdmissions;
use weld_hoist_core::{
    HoistEndpoint, HoistEndpointCommand, HoistPortResult, HoistSourcePort, SourcePortCommand,
    SourceRelayAdapter, relocated_surface,
};
use weld_hoist_protocol::DestinationEnvelope;
use weld_media::VideoCodec;

#[derive(Clone)]
pub struct SourceSessionEndpoint<T: SourceConnection> {
    pub session: SessionId,
    peer: T,
    source: ClientSourceId,
    destination: ClientSourceId,
}
struct RoutedCommand {
    session: SessionId,
    command: HoistEndpointCommand,
}
impl<T: SourceConnection> HoistEndpoint for SourceSessionEndpoint<T> {
    fn is_available(&self) -> bool {
        self.peer.is_connected()
    }
    fn has_local_receiver(&self) -> bool {
        false
    }
    fn destination(&self, source: ClientSurfaceId) -> ClientSurfaceId {
        relocated_surface(self.destination, source)
    }
    fn map(
        &self,
        session: weld_hoist_core::HoistSessionId,
        source: ClientSurfaceId,
    ) -> ClientAdapterCommandEnvelope {
        ClientAdapterCommandEnvelope::new(
            self.source,
            RoutedCommand {
                session: self.session,
                command: HoistEndpointCommand::Map { session, source },
            },
        )
    }
    fn unmap(&self, source: ClientSurfaceId) -> ClientAdapterCommandEnvelope {
        ClientAdapterCommandEnvelope::new(
            self.source,
            RoutedCommand {
                session: self.session,
                command: HoistEndpointCommand::Unmap { source },
            },
        )
    }
}

/// Session identity and its core-authorized endpoint, ready for host registration.
pub type SessionEndpoint<T> = (SessionId, AuthorizedEndpoint<SourceSessionEndpoint<T>>);

#[derive(Clone)]
pub struct SourceSessionEndpoints<T: SourceConnection>(Arc<Mutex<Vec<SessionEndpoint<T>>>>);
impl<T: SourceConnection> Default for SourceSessionEndpoints<T> {
    fn default() -> Self {
        Self(Arc::default())
    }
}
impl<T: SourceConnection> SourceSessionEndpoints<T> {
    pub fn take(&self) -> Vec<SessionEndpoint<T>> {
        self.0
            .lock()
            .map(|mut entries| std::mem::take(&mut *entries))
            .unwrap_or_default()
    }
}

pub fn source_sessions_registration<T: SourceConnection>(
    sessions: SourceAdmissions<T>,
    source: ClientSourceId,
    upstream: ClientSourceId,
    budget: SharedBitrateBudget,
    factory: impl Fn(VideoCodec) -> Result<Box<dyn EncodeBackend>> + 'static,
) -> (ClientAdapterRegistration, SourceSessionEndpoints<T>) {
    let endpoints = SourceSessionEndpoints::default();
    (
        ClientAdapterRegistration::new(
            ClientSourceDescriptor::new(source, ClientProvenance::Relocated),
            SourceSessionAdapter {
                sessions,
                source,
                inventory: SourceRelayAdapter::new(upstream, IdlePort),
                relays: BTreeMap::new(),
                endpoints: endpoints.clone(),
                budget,
                factory: Box::new(factory),
                pending_effects: Vec::new(),
            },
            ControlOnlyClientImporter,
        ),
        endpoints,
    )
}
struct ActiveRelay<T: SourceConnection> {
    peer: T,
    relay: SourceRelayAdapter,
    effects_drained: bool,
    claims_drained: bool,
}
struct SourceSessionAdapter<T: SourceConnection> {
    sessions: SourceAdmissions<T>,
    source: ClientSourceId,
    inventory: SourceRelayAdapter,
    relays: BTreeMap<SessionId, ActiveRelay<T>>,
    endpoints: SourceSessionEndpoints<T>,
    budget: SharedBitrateBudget,
    factory: Box<dyn Fn(VideoCodec) -> Result<Box<dyn EncodeBackend>>>,
    pending_effects: Vec<ClientAdapterEffect>,
}
impl<T: SourceConnection> ClientAdapter for SourceSessionAdapter<T> {
    fn input_source(&self) -> Option<ClientSourceId> {
        self.inventory.input_source()
    }
    fn next_deadline(&self) -> Option<Instant> {
        self.relays
            .values()
            .filter_map(|active| active.relay.next_deadline())
            .min()
    }
    fn presentation_source(&self) -> Option<ClientSourceId> {
        self.inventory.presentation_source()
    }
    fn drain_events(&mut self, events: &mut ClientEventQueue) {
        for (session, peer, access, codec) in self.sessions.take() {
            let result = (|| -> Result<()> {
                let backend = (self.factory)(codec)?;
                let port = EncodedSourcePort::configured(
                    peer.clone(),
                    backend,
                    EncodedSourceOptions {
                        bitrate_budget: Some(self.budget.clone()),
                        access_unit_dump: None,
                    },
                )?;
                let relay = self.inventory.fork(port).with_access(access.clone());
                // Each connection receives a fresh policy namespace; remote shells
                // retain their own destination namespace in their own runtime.
                let destination = ClientSourceId::new(
                    session
                        .0
                        .checked_add(100)
                        .ok_or_else(|| anyhow::anyhow!("session namespaces exhausted"))?,
                );
                self.relays.insert(
                    session,
                    ActiveRelay {
                        peer: peer.clone(),
                        relay,
                        effects_drained: false,
                        claims_drained: false,
                    },
                );
                self.endpoints
                    .0
                    .lock()
                    .map_err(|_| anyhow::anyhow!("endpoint handoff poisoned"))?
                    .push((
                        session,
                        authorize_endpoint(
                            SourceSessionEndpoint {
                                session,
                                peer: peer.clone(),
                                source: self.source,
                                destination,
                            },
                            access,
                        ),
                    ));
                Ok(())
            })();
            if let Err(error) = result {
                tracing::warn!(%error, "could not prepare encoded session");
                peer.disconnect();
            }
        }
        for active in self.relays.values_mut() {
            active.effects_drained = false;
            active.claims_drained = false;
            active.relay.drain_events(events);
        }
    }
    fn apply_request(&mut self, _: ClientRequest) {}
    fn apply_input(&mut self, _: ClientInputEvent) {}
    fn host_focus_lost(&mut self, _: u32) {}
    fn apply_command(&mut self, command: ClientAdapterCommandEnvelope) {
        if let Ok(command) = command.downcast::<RoutedCommand>()
            && let Some(active) = self.relays.get_mut(&command.session)
            && active.peer.is_connected()
        {
            active
                .relay
                .apply_command(ClientAdapterCommandEnvelope::new(
                    self.source,
                    command.command,
                ));
        }
    }
    fn observe_event(&mut self, event: &ClientSurfaceEvent) {
        self.inventory.observe_event(event);
        for active in self.relays.values_mut() {
            active.relay.observe_event(event);
        }
    }
    fn observe_cursor_update(&mut self, update: &ClientCursorUpdate) {
        self.inventory.observe_cursor_update(update);
        for active in self.relays.values_mut() {
            active.relay.observe_cursor_update(update);
        }
    }
    fn observe_retired_buffer(&mut self, buffer: ClientBufferId) {
        self.inventory.observe_retired_buffer(buffer);
        for active in self.relays.values_mut() {
            active.relay.observe_retired_buffer(buffer);
        }
    }
    fn drain_presentation_claims(&mut self, updates: &mut Vec<ClientPresentationUpdate>) {
        for active in self.relays.values_mut() {
            active.relay.drain_presentation_claims(updates);
            active.claims_drained = true;
        }
        self.relays
            .retain(|_, active| active.peer.is_connected() || !active.effects_drained);
    }
    fn drain_controller_effects(&mut self, effects: &mut Vec<(u64, ClientAdapterEffect)>) {
        for (session, active) in &mut self.relays {
            active.relay.drain_effects(&mut self.pending_effects);
            effects.extend(
                self.pending_effects
                    .drain(..)
                    .map(|effect| (session.0, effect)),
            );
            active.effects_drained = true;
        }
        // Failure releases and presentation withdrawals drain before retirement.
        self.relays
            .retain(|_, active| active.peer.is_connected() || !active.claims_drained);
    }
}
impl<T: SourceConnection> Drop for SourceSessionAdapter<T> {
    fn drop(&mut self) {
        for active in self.relays.values() {
            active.peer.disconnect();
        }
    }
}

struct IdlePort;
impl HoistSourcePort for IdlePort {
    fn ready(&self) -> bool {
        false
    }
    fn submit(&mut self, _: SourcePortCommand) -> HoistPortResult<()> {
        Ok(())
    }
    fn poll(&mut self) -> HoistPortResult<Vec<DestinationEnvelope>> {
        Ok(Vec::new())
    }
    fn accept_destination(&mut self, _: &DestinationEnvelope) -> HoistPortResult<()> {
        Ok(())
    }
    fn progress_after_destination(&mut self) -> HoistPortResult<()> {
        Ok(())
    }
    fn effects_drained(&mut self) {}
    fn disconnect(&mut self) {}
}
