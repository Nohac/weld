//! In-process binding for the shared hoist relays.

use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use weld_client::{
    ClientAdapter, ClientAdapterCommandEnvelope, ClientAdapterEffect, ClientAdapterRegistration,
    ClientBufferId, ClientBufferUseId, ClientEventQueue, ClientInputEvent, ClientRequest,
    ClientRouteAliasUpdate, ClientSourceDescriptor, ClientSourceId, ClientSurfaceEvent,
    ClientSurfaceEventKind, PassthroughClientImporter, SurfaceBufferChange,
};
use weld_hoist_protocol::DestinationEnvelope;

use crate::{
    DestinationPortCommand, DestinationPortEvent, DestinationPortRecord, DestinationRelayAdapter,
    HoistDestinationPort, HoistPortResult, HoistSourcePort, LoopbackEndpoint, SourcePortCommand,
    SourceRelayAdapter,
};

#[derive(Default)]
struct LoopbackQueues {
    source_to_destination: VecDeque<DestinationPortRecord>,
    destination_to_source: VecDeque<DestinationEnvelope>,
}

struct LoopbackSourcePort {
    queues: Rc<RefCell<LoopbackQueues>>,
}

impl HoistSourcePort for LoopbackSourcePort {
    fn submit(&mut self, command: SourcePortCommand) -> HoistPortResult<()> {
        let record = match command {
            SourcePortCommand::FocusCleared => return Ok(()),
            SourcePortCommand::Cursor {
                session,
                update,
                sequence,
            } => DestinationPortRecord {
                session,
                event: DestinationPortEvent::Cursor { update, sequence },
            },
            SourcePortCommand::MapSurface { session, surface } => DestinationPortRecord {
                session,
                event: DestinationPortEvent::MappedSurface(surface),
            },
            SourcePortCommand::Surface { session, event } => DestinationPortRecord {
                session,
                event: DestinationPortEvent::Surface(event),
            },
            SourcePortCommand::WithdrawSurface { session, surface } => DestinationPortRecord {
                session,
                event: DestinationPortEvent::WithdrawSurface(surface),
            },
            SourcePortCommand::RetireUpstreamBuffer(_) => return Ok(()),
        };
        self.queues
            .borrow_mut()
            .source_to_destination
            .push_back(record);
        Ok(())
    }

    fn poll(&mut self) -> HoistPortResult<Vec<DestinationEnvelope>> {
        Ok(self
            .queues
            .borrow_mut()
            .destination_to_source
            .drain(..)
            .collect())
    }

    fn accept_destination(&mut self, _envelope: &DestinationEnvelope) -> HoistPortResult<()> {
        Ok(())
    }

    fn effects_drained(&mut self) {}

    fn progress_after_destination(&mut self) -> HoistPortResult<()> {
        Ok(())
    }

    fn disconnect(&mut self) {
        let mut queues = self.queues.borrow_mut();
        queues.source_to_destination.clear();
        queues.destination_to_source.clear();
    }
}

struct LoopbackDestinationPort {
    queues: Rc<RefCell<LoopbackQueues>>,
    destination: ClientSourceId,
    next_buffer_use: Option<u64>,
    aliases: Rc<RefCell<VecDeque<ClientRouteAliasUpdate>>>,
}

impl LoopbackDestinationPort {
    fn relay_event(&mut self, mut event: ClientSurfaceEvent) -> ClientSurfaceEvent {
        let ClientSurfaceEventKind::Commit(commit) = &mut event.kind else {
            return event;
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
            let destination_buffer = ClientBufferId::new(self.destination, buffer.buffer().local());
            let destination_use = ClientBufferUseId::new(self.destination, use_local);
            if let Ok(relayed) = buffer.clone().relay(destination_buffer, destination_use) {
                *buffer = relayed;
            } else {
                update.change = SurfaceBufferChange::Retained {
                    metadata: *metadata,
                };
            }
        }
        event
    }
}

impl HoistDestinationPort for LoopbackDestinationPort {
    fn poll(&mut self) -> HoistPortResult<Vec<DestinationPortRecord>> {
        let records = self
            .queues
            .borrow_mut()
            .source_to_destination
            .drain(..)
            .collect::<Vec<_>>();
        Ok(records
            .into_iter()
            .map(|mut record| {
                if let DestinationPortEvent::Surface(event) = record.event {
                    record.event = DestinationPortEvent::Surface(self.relay_event(event));
                }
                record
            })
            .collect())
    }

    fn submit(&mut self, command: DestinationPortCommand) -> HoistPortResult<()> {
        match command {
            DestinationPortCommand::FocusCleared => {}
            DestinationPortCommand::Message(envelope) => self
                .queues
                .borrow_mut()
                .destination_to_source
                .push_back(envelope),
            DestinationPortCommand::RouteMapped {
                source,
                destination,
            } => self.aliases.borrow_mut().push_back(ClientRouteAliasUpdate {
                destination,
                source: Some(source),
            }),
            DestinationPortCommand::RouteUnmapped { destination } => {
                self.aliases.borrow_mut().push_back(ClientRouteAliasUpdate {
                    destination,
                    source: None,
                });
            }
        }
        Ok(())
    }

    fn drain_route_alias_updates(&mut self, updates: &mut Vec<ClientRouteAliasUpdate>) {
        updates.extend(self.aliases.borrow_mut().drain(..));
    }

    fn disconnect(&mut self) {
        let mut queues = self.queues.borrow_mut();
        queues.source_to_destination.clear();
        queues.destination_to_source.clear();
    }
}

struct LoopbackClientAdapter {
    source: SourceRelayAdapter,
    destination: DestinationRelayAdapter,
    events: ClientEventQueue,
}

impl ClientAdapter for LoopbackClientAdapter {
    fn next_deadline(&self) -> Option<std::time::Instant> {
        self.source.next_deadline()
    }
    fn observe_cursor_update(&mut self, update: &weld_client::ClientCursorUpdate) {
        self.source.observe_cursor_update(update);
        self.destination.drain_events(&mut self.events);
    }

    fn drain_cursor_updates(&mut self, updates: &mut Vec<weld_client::ClientCursorUpdate>) {
        self.destination.drain_cursor_updates(updates);
    }
    fn drain_events(&mut self, events: &mut ClientEventQueue) {
        // Pump source commands into the in-process queue before the destination
        // drains it. Route aliases therefore exist when the runtime collects
        // alias updates later in this same drain.
        self.source.drain_events(events);
        self.destination.drain_events(&mut self.events);
        while let Some(event) = self.events.pop_front() {
            events.push(event);
        }
    }

    fn apply_request(&mut self, request: ClientRequest) {
        self.destination.apply_request(request);
    }

    fn apply_input(&mut self, event: ClientInputEvent) {
        self.destination.apply_input(event);
    }

    fn apply_command(&mut self, command: ClientAdapterCommandEnvelope) {
        self.source.apply_command(command);
        self.destination.drain_events(&mut self.events);
    }

    fn host_focus_lost(&mut self, time: u32) {
        self.source.host_focus_lost(time);
        self.destination.host_focus_lost(time);
    }

    fn observe_event(&mut self, event: &ClientSurfaceEvent) {
        self.source.observe_event(event);
        self.destination.drain_events(&mut self.events);
    }

    fn drain_effects(&mut self, effects: &mut Vec<ClientAdapterEffect>) {
        self.source.drain_effects(effects);
    }

    fn observe_retired_buffer(&mut self, buffer: ClientBufferId) {
        self.source.observe_retired_buffer(buffer);
    }

    fn drain_route_alias_updates(&mut self, updates: &mut Vec<ClientRouteAliasUpdate>) {
        self.destination.drain_route_alias_updates(updates);
    }
}

pub(crate) fn registration(
    upstream: ClientSourceId,
    descriptor: ClientSourceDescriptor,
) -> ClientAdapterRegistration {
    let queues = Rc::new(RefCell::new(LoopbackQueues::default()));
    let aliases = Rc::new(RefCell::new(VecDeque::new()));
    let source = SourceRelayAdapter::new(
        upstream,
        LoopbackSourcePort {
            queues: queues.clone(),
        },
    );
    let destination = DestinationRelayAdapter::new(
        upstream,
        descriptor,
        LoopbackDestinationPort {
            queues,
            destination: descriptor.id,
            next_buffer_use: Some(1),
            aliases: aliases.clone(),
        },
    );
    ClientAdapterRegistration::new(
        descriptor,
        LoopbackClientAdapter {
            source,
            destination,
            events: ClientEventQueue::default(),
        },
        PassthroughClientImporter,
    )
}

pub(crate) fn endpoint(destination: ClientSourceId) -> LoopbackEndpoint {
    LoopbackEndpoint {
        source: destination,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_client::{
        ClientCursor, ClientCursorUpdate, ClientId, ClientProvenance, ClientSurfaceId,
        ClientSurfaceRole, CursorIcon, ToplevelState, WindowDecoration,
    };

    #[test]
    fn loopback_adapter_drains_relocated_feedback_without_using_a_runtime_alias() {
        let source = ClientSourceId::new(0);
        let destination = ClientSourceId::new(1);
        let surface = ClientSurfaceId::new(ClientId::new(source, 1), 7);
        let mut adapter = registration(
            source,
            ClientSourceDescriptor::new(destination, ClientProvenance::Relocated),
        )
        .into_parts()
        .runtime
        .driver;
        adapter.observe_event(&ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                parent: None,
                decoration: WindowDecoration::ClientSide,
            })),
        });
        adapter.apply_command(endpoint(destination).map(crate::HoistSessionId::new(1), surface));
        adapter.observe_cursor_update(&ClientCursorUpdate {
            surface,
            cursor: ClientCursor::Named(CursorIcon::Text),
        });
        let mut updates = Vec::new();
        adapter.drain_cursor_updates(&mut updates);
        assert_eq!(
            updates,
            vec![ClientCursorUpdate {
                surface: endpoint(destination).destination(surface),
                cursor: ClientCursor::Named(CursorIcon::Text)
            }]
        );
    }
}
