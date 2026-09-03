use std::{
    collections::{HashMap, HashSet},
    os::fd::OwnedFd,
};

use tracing::{error, warn};
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferUseId, ClientSurfaceEvent,
    WireClientSurfaceEvent,
};
use weld_hoist_core::{HoistPortResult, HoistSourcePort, SourcePortCommand};
use weld_hoist_protocol::{
    DestinationEnvelope, DestinationMessage, HoistSessionId, SourceEnvelope, SourceMessage,
};

use crate::{
    LocalBufferContent, LocalDestinationPacket, LocalPacketConnection, LocalSourcePacket,
    export_local_buffer,
};

pub(crate) struct LocalSourcePort {
    connection: LocalPacketConnection,
    published_buffers: HashMap<ClientBufferId, HashSet<HoistSessionId>>,
    pending_uses: HashMap<ClientBufferUseId, (HoistSessionId, ClientBufferLease)>,
    retirements: Vec<ClientBufferUseId>,
}

impl LocalSourcePort {
    pub(crate) fn new(connection: LocalPacketConnection) -> Self {
        Self {
            connection,
            published_buffers: HashMap::new(),
            pending_uses: HashMap::new(),
            retirements: Vec::new(),
        }
    }

    fn send_event(
        &mut self,
        session: HoistSessionId,
        event: ClientSurfaceEvent,
    ) -> HoistPortResult<()> {
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
                if let Err(error) = self.queue_source(
                    SourceEnvelope {
                        session,
                        message: SourceMessage::Surface(event),
                    },
                    file_descriptors,
                ) {
                    self.rollback_export(session, &added_buffer_sessions, &added_uses);
                    return Err(error);
                }
            }
            Err(error) => {
                error!(%error, ?session, "could not export a hoisted client buffer");
                self.rollback_export(session, &added_buffer_sessions, &added_uses);
                self.connection
                    .record_failure(crate::TransportError::Protocol(error.to_string()));
                return Err(protocol_error(error));
            }
        }
        Ok(())
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

    fn queue_source(
        &self,
        packet: LocalSourcePacket,
        file_descriptors: Vec<OwnedFd>,
    ) -> HoistPortResult<()> {
        self.connection
            .queue(&packet, file_descriptors)
            .map_err(|error| {
                warn!(%error, "could not queue a local hoist source packet");
                let message = error.to_string();
                self.connection.record_failure(error);
                protocol_error(message)
            })
    }

    fn retire_session_buffers(&mut self, session: HoistSessionId) -> HoistPortResult<()> {
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
                SourceEnvelope {
                    session,
                    message: SourceMessage::BufferRetired { buffer },
                },
                Vec::new(),
            )?;
        }
        Ok(())
    }

    fn retire_buffer(&mut self, buffer: ClientBufferId) -> HoistPortResult<()> {
        let Some(sessions) = self.published_buffers.remove(&buffer) else {
            return Ok(());
        };
        for session in sessions {
            self.queue_source(
                SourceEnvelope {
                    session,
                    message: SourceMessage::BufferRetired { buffer },
                },
                Vec::new(),
            )?;
        }
        Ok(())
    }
}

impl HoistSourcePort for LocalSourcePort {
    fn submit(&mut self, command: SourcePortCommand) -> HoistPortResult<()> {
        match command {
            SourcePortCommand::MapSurface { session, surface } => self.queue_source(
                SourceEnvelope {
                    session,
                    message: SourceMessage::Mapped { surface },
                },
                Vec::new(),
            ),
            SourcePortCommand::Surface { session, event } => self.send_event(session, event),
            SourcePortCommand::WithdrawSurface { session, surface } => {
                self.queue_source(
                    SourceEnvelope {
                        session,
                        message: SourceMessage::Withdraw { surface },
                    },
                    Vec::new(),
                )?;
                self.retire_session_buffers(session)
            }
            SourcePortCommand::RetireUpstreamBuffer(buffer) => self.retire_buffer(buffer),
        }
    }

    fn poll(&mut self) -> HoistPortResult<Vec<DestinationEnvelope>> {
        let packets = self
            .connection
            .drain::<LocalDestinationPacket>()
            .map_err(|error| {
                warn!(%error, "local hoist destination transport failed");
                Box::new(error) as weld_hoist_core::HoistPortError
            })?;
        let mut envelopes = Vec::with_capacity(packets.len());
        for packet in packets {
            if !packet.file_descriptors.is_empty() {
                // Ignoring descriptor-bearing control would lose FD ownership
                // and desynchronize all later descriptor indices.
                let message = "destination control packet attached descriptors".to_owned();
                self.connection
                    .record_failure(crate::TransportError::Protocol(message.clone()));
                return Err(protocol_error(message));
            }
            envelopes.push(packet.message);
        }
        Ok(envelopes)
    }

    fn accept_destination(&mut self, envelope: &DestinationEnvelope) -> HoistPortResult<()> {
        match &envelope.message {
            DestinationMessage::BufferReleased { use_id } => {
                if self
                    .pending_uses
                    .get(use_id)
                    .is_some_and(|(session, _)| *session == envelope.session)
                {
                    self.retirements.push(*use_id);
                } else {
                    warn!(?use_id, session = ?envelope.session, "ignored an unknown local hoist buffer release");
                }
            }
            DestinationMessage::EncodedCommitFinished { .. } => {
                return Err(protocol_error(
                    "native hoist peer sent an encoded commit outcome",
                ));
            }
            DestinationMessage::Request(_)
            | DestinationMessage::Input(_)
            | DestinationMessage::Reclaim => {}
        }
        Ok(())
    }

    fn effects_drained(&mut self) {
        for use_id in self.retirements.drain(..) {
            self.pending_uses.remove(&use_id);
        }
    }

    fn disconnect(&mut self) {
        if !self.connection.is_disconnected() {
            self.connection
                .record_failure(crate::TransportError::Protocol(
                    "local source relay rejected protocol state".to_owned(),
                ));
        }
        self.pending_uses.clear();
    }
}

fn protocol_error(error: impl std::fmt::Display) -> weld_hoist_core::HoistPortError {
    Box::new(crate::TransportError::Protocol(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use weld_client::{
        ClientAdapter, ClientAdapterCommandEnvelope, ClientAdapterEffect, ClientBufferId,
        ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, ClientCommitRevision,
        ClientEventQueue, ClientId, ClientRequest, ClientSourceId, ClientSurfaceCommit,
        ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceId, ClientSurfaceRequest,
        ClientSurfaceRequestKind, Extent, SurfaceBufferChange, SurfaceBufferUpdate, SurfaceLayerId,
        WireClientSurfaceEventKind, WireSurfaceBufferChange,
    };
    use weld_hoist_core::{HoistEndpointCommand, SourceRelayAdapter};

    use super::*;

    #[test]
    fn shm_transfer_uses_one_descriptor_without_entering_the_reuse_cache() {
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
            Rc::new(weld_core::dmabuf::DirectClientBufferAccess::Shm(
                weld_core::dmabuf::WaylandShmBuffer {
                    bgra_pixels: vec![1, 2, 3, 4],
                },
            )),
            |_| {},
        )
        .expect("matching source");
        let mut port = LocalSourcePort::new(source_connection.clone());

        port.submit(SourcePortCommand::Surface {
            session,
            event: ClientSurfaceEvent {
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
        })
        .expect("source surface");
        source_connection.pump().expect("source send");
        let packets = destination_connection
            .drain::<LocalSourcePacket>()
            .expect("SHM packet");

        assert!(port.published_buffers.is_empty());
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].file_descriptors.len(), 1);
        let SourceMessage::Surface(event) = &packets[0].message.message else {
            panic!("expected surface packet");
        };
        let WireClientSurfaceEventKind::Commit(commit) = &event.kind else {
            panic!("expected surface commit");
        };
        assert!(matches!(
            &commit.buffers[0].change,
            WireSurfaceBufferChange::Replaced {
                buffer: crate::LocalBuffer {
                    content: LocalBufferContent::Shm(_),
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn retired_native_buffer_is_forwarded_once() {
        let (source_connection, destination_connection) =
            LocalPacketConnection::pair().expect("transport pair");
        let source = ClientSourceId::new(0);
        let session = HoistSessionId::new(1);
        let buffer = ClientBufferId::new(source, 2);
        let mut port = LocalSourcePort::new(source_connection.clone());
        port.published_buffers
            .insert(buffer, HashSet::from([session]));

        port.submit(SourcePortCommand::RetireUpstreamBuffer(buffer))
            .expect("buffer retirement");
        source_connection.pump().expect("source send");
        let packets = destination_connection
            .drain::<LocalSourcePacket>()
            .expect("retirement packet");

        assert!(matches!(
            packets[0].message.message,
            SourceMessage::BufferRetired { buffer: retired } if retired == buffer
        ));
        assert!(!port.published_buffers.contains_key(&buffer));
    }

    #[test]
    fn destination_configure_crosses_the_local_binding_into_an_authorized_effect() {
        let (source_connection, destination_connection) =
            LocalPacketConnection::pair().expect("transport pair");
        let source_id = ClientSourceId::new(0);
        let surface = ClientSurfaceId::new(ClientId::new(source_id, 1), 2);
        let session = HoistSessionId::new(3);
        let mut adapter =
            SourceRelayAdapter::new(source_id, LocalSourcePort::new(source_connection.clone()));
        adapter.apply_command(ClientAdapterCommandEnvelope::new(
            ClientSourceId::new(1),
            HoistEndpointCommand::Map {
                session,
                source: surface,
            },
        ));
        source_connection.pump().expect("mapped surface send");
        let _ = destination_connection
            .drain::<LocalSourcePacket>()
            .expect("mapped surface record");

        destination_connection
            .queue(
                &DestinationEnvelope {
                    session,
                    message: DestinationMessage::Request(ClientRequest::Surface(
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
        destination_connection
            .pump()
            .expect("destination request send");
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
}
