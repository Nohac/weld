//! Runtime-owned media relays for dynamically connected desktop viewers.
use crate::{
    IrohSourcePeer,
    pairing::{DesktopSessions, SessionId},
};
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
use weld_hoist_core::{
    HoistEndpoint, HoistEndpointCommand, HoistPortResult, HoistSourcePort, SourcePortCommand,
    SourceRelayAdapter, relocated_surface,
};
use weld_hoist_encoded::{
    EncodeBackend, EncodedSourceOptions, EncodedSourcePort, EncodedSourceTransport,
    SharedBitrateBudget,
};
use weld_hoist_protocol::DestinationEnvelope;
use weld_media::VideoCodec;

#[derive(Clone)]
pub struct DesktopEndpoint {
    pub session: SessionId,
    peer: IrohSourcePeer,
    source: ClientSourceId,
    destination: ClientSourceId,
}
struct RoutedCommand {
    session: SessionId,
    command: HoistEndpointCommand,
}
impl HoistEndpoint for DesktopEndpoint {
    fn is_available(&self) -> bool {
        self.peer.is_available()
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

#[derive(Clone, Default)]
pub struct DesktopEndpoints(Arc<Mutex<Vec<DesktopEndpoint>>>);
impl DesktopEndpoints {
    pub fn take(&self) -> Vec<DesktopEndpoint> {
        self.0
            .lock()
            .map(|mut entries| std::mem::take(&mut *entries))
            .unwrap_or_default()
    }
}

pub fn desktop_source_registration(
    sessions: DesktopSessions,
    source: ClientSourceId,
    upstream: ClientSourceId,
    budget: SharedBitrateBudget,
    factory: impl Fn(VideoCodec) -> Result<Box<dyn EncodeBackend>> + 'static,
) -> (ClientAdapterRegistration, DesktopEndpoints) {
    let endpoints = DesktopEndpoints::default();
    (
        ClientAdapterRegistration::new(
            ClientSourceDescriptor::new(source, ClientProvenance::Relocated),
            DesktopAdapter {
                sessions,
                source,
                inventory: SourceRelayAdapter::new(upstream, IdlePort),
                relays: BTreeMap::new(),
                endpoints: endpoints.clone(),
                budget,
                factory: Box::new(factory),
            },
            ControlOnlyClientImporter,
        ),
        endpoints,
    )
}
struct ActiveRelay {
    peer: IrohSourcePeer,
    relay: SourceRelayAdapter,
    effects_drained: bool,
    claims_drained: bool,
}
struct DesktopAdapter {
    sessions: DesktopSessions,
    source: ClientSourceId,
    inventory: SourceRelayAdapter,
    relays: BTreeMap<SessionId, ActiveRelay>,
    endpoints: DesktopEndpoints,
    budget: SharedBitrateBudget,
    factory: Box<dyn Fn(VideoCodec) -> Result<Box<dyn EncodeBackend>>>,
}
impl ClientAdapter for DesktopAdapter {
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
        for (session, peer) in self.sessions.take_peers() {
            let result = (|| -> Result<()> {
                let backend = (self.factory)(peer.codec())?;
                let port = EncodedSourcePort::configured(
                    peer.clone(),
                    backend,
                    EncodedSourceOptions {
                        bitrate_budget: Some(self.budget.clone()),
                        access_unit_dump: None,
                    },
                )?;
                let relay = self.inventory.fork(port);
                // Each connection receives a fresh policy namespace; remote shells
                // retain their own destination namespace in their own runtime.
                let destination = ClientSourceId::new(
                    session
                        .0
                        .checked_add(100)
                        .ok_or_else(|| anyhow::anyhow!("desktop namespaces exhausted"))?,
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
                    .push(DesktopEndpoint {
                        session,
                        peer: peer.clone(),
                        source: self.source,
                        destination,
                    });
                Ok(())
            })();
            if let Err(error) = result {
                tracing::warn!(%error, "could not prepare desktop hoist");
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
            && active.peer.is_available()
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
            .retain(|_, active| active.peer.is_available() || !active.effects_drained);
    }
    fn drain_effects(&mut self, effects: &mut Vec<ClientAdapterEffect>) {
        for active in self.relays.values_mut() {
            active.relay.drain_effects(effects);
            active.effects_drained = true;
        }
        // Failure releases and presentation withdrawals drain before retirement.
        self.relays
            .retain(|_, active| active.peer.is_available() || !active.claims_drained);
    }
}
impl Drop for DesktopAdapter {
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
