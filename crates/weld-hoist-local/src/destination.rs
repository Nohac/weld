use std::{collections::HashMap, os::fd::OwnedFd, rc::Rc};

use tracing::warn;
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId,
    ClientSourceDescriptor, WireClientSurfaceEvent, WireClientSurfaceEventKind,
    WireSurfaceBufferChange,
};
use weld_core::dmabuf::{DirectClientBufferAccess, DmabufAccess, DmabufContext};
use weld_hoist_core::{
    DestinationPortCommand, DestinationPortEvent, DestinationPortRecord, HoistDestinationPort,
    HoistPortResult,
};
use weld_hoist_protocol::{DestinationEnvelope, DestinationMessage, HoistSessionId, SourceMessage};

use crate::{
    LocalBuffer, LocalBufferContent, LocalPacketConnection, LocalSourcePacket,
    ensure_descriptors_consumed, import_local_dmabuf, import_local_shm,
};

struct ImportedDmabuf {
    local: u64,
    access: DmabufAccess,
}

pub(crate) struct LocalDestinationPort {
    connection: LocalPacketConnection,
    descriptor: ClientSourceDescriptor,
    dmabuf: Option<DmabufContext>,
    buffers: HashMap<ClientBufferId, ImportedDmabuf>,
    next_buffer: Option<u64>,
    next_use: Option<u64>,
}

impl LocalDestinationPort {
    pub(crate) fn new(
        connection: LocalPacketConnection,
        descriptor: ClientSourceDescriptor,
        dmabuf: DmabufContext,
    ) -> Self {
        Self {
            connection,
            descriptor,
            dmabuf: Some(dmabuf),
            buffers: HashMap::new(),
            next_buffer: Some(1),
            next_use: Some(1),
        }
    }

    #[cfg(test)]
    fn new_without_dmabuf(
        connection: LocalPacketConnection,
        descriptor: ClientSourceDescriptor,
    ) -> Self {
        Self {
            connection,
            descriptor,
            dmabuf: None,
            buffers: HashMap::new(),
            next_buffer: Some(1),
            next_use: Some(1),
        }
    }

    fn apply_source_packet(
        &mut self,
        packet: LocalSourcePacket,
        file_descriptors: Vec<OwnedFd>,
        records: &mut Vec<DestinationPortRecord>,
    ) -> HoistPortResult<()> {
        match packet.message {
            SourceMessage::Mapped { surface } => records.push(DestinationPortRecord {
                session: packet.session,
                event: DestinationPortEvent::MappedSurface(surface),
            }),
            SourceMessage::Surface(event) => {
                self.import_native(packet.session, event, file_descriptors, records)?;
            }
            SourceMessage::BufferRetired { buffer } => {
                if let Some(imported) = self.buffers.remove(&buffer) {
                    let Some(dmabuf) = &self.dmabuf else {
                        return Err(self.protocol_failure(
                            "native buffer cache exists without a DMA-BUF context".to_owned(),
                        ));
                    };
                    dmabuf.remove_external(&imported.access);
                }
            }
            SourceMessage::Withdraw { surface } => {
                records.push(DestinationPortRecord {
                    session: packet.session,
                    event: DestinationPortEvent::WithdrawSurface(surface),
                });
            }
            SourceMessage::Ended => records.push(DestinationPortRecord {
                session: packet.session,
                event: DestinationPortEvent::Ended,
            }),
        }
        Ok(())
    }

    fn import_native(
        &mut self,
        session: HoistSessionId,
        event: WireClientSurfaceEvent<LocalBuffer>,
        file_descriptors: Vec<OwnedFd>,
        records: &mut Vec<DestinationPortRecord>,
    ) -> HoistPortResult<()> {
        let mut unreleased_uses = wire_buffer_uses(&event);
        let mut descriptors = file_descriptors.into_iter().map(Some).collect::<Vec<_>>();
        let event = event.try_into_client(|buffer, metadata| {
            let source_use = buffer.use_id;
            let imported = self.import_buffer(session, buffer, metadata, &mut descriptors);
            if imported.is_ok() {
                unreleased_uses.retain(|use_id| *use_id != source_use);
            }
            imported
        });
        match event.and_then(|event| {
            ensure_descriptors_consumed(&descriptors)?;
            Ok(event)
        }) {
            Ok(event) => records.push(DestinationPortRecord {
                session,
                event: DestinationPortEvent::Surface(event),
            }),
            Err(error) => {
                warn!(%error, "rejected a local hoist surface packet");
                for use_id in unreleased_uses {
                    self.queue_destination(session, DestinationMessage::BufferReleased { use_id })?;
                }
            }
        }
        Ok(())
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
                let dmabuf = self
                    .dmabuf
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("DMA-BUF import requires a context"))?;
                ImportedAccess::Dmabuf(dmabuf.import_external(external)?)
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
            LocalBufferContent::Encoded(_) => {
                anyhow::bail!("encoded buffer entered the native destination importer")
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
            let packet = DestinationEnvelope {
                session,
                message: DestinationMessage::BufferReleased { use_id: source_use },
            };
            if let Err(error) = connection.queue(&packet, Vec::new()) {
                warn!(%error, message_kind = "buffer-released", "could not release a local hoist buffer use");
            }
        };
        let buffer = ClientBufferId::new(self.descriptor.id, local);
        let use_id = ClientBufferUseId::new(self.descriptor.id, use_local);
        match access {
            ImportedAccess::Dmabuf(access) => self
                .dmabuf
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("DMA-BUF lease requires a context"))?
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

    fn queue_destination(
        &self,
        session: HoistSessionId,
        message: DestinationMessage,
    ) -> HoistPortResult<()> {
        let message_kind = message.kind();
        self.connection
            .queue(&DestinationEnvelope { session, message }, Vec::new())
            .map_err(|error| {
                warn!(%error, message_kind, "could not queue a local hoist destination packet");
                protocol_error(error)
            })
    }

    fn protocol_failure(&self, message: String) -> weld_hoist_core::HoistPortError {
        self.connection
            .record_failure(crate::TransportError::Protocol(message.clone()));
        Box::new(crate::TransportError::Protocol(message))
    }
}

impl HoistDestinationPort for LocalDestinationPort {
    fn poll(&mut self) -> HoistPortResult<Vec<DestinationPortRecord>> {
        let packets = self
            .connection
            .drain::<LocalSourcePacket>()
            .map_err(|error| {
                warn!(%error, "local hoist source transport failed");
                protocol_error(error)
            })?;
        let mut records = Vec::new();
        for packet in packets {
            self.apply_source_packet(packet.message, packet.file_descriptors, &mut records)?;
        }
        Ok(records)
    }

    fn submit(&mut self, command: DestinationPortCommand) -> HoistPortResult<()> {
        match command {
            DestinationPortCommand::Message(envelope) => {
                let message_kind = envelope.message.kind();
                self.connection
                    .queue(&envelope, Vec::new())
                    .map_err(|error| {
                        warn!(%error, message_kind, "could not queue a local hoist destination packet");
                        protocol_error(error)
                    })
            }
            DestinationPortCommand::RouteMapped { .. }
            | DestinationPortCommand::RouteUnmapped { .. } => Ok(()),
        }
    }

    fn disconnect(&mut self) {
        if !self.connection.is_disconnected() {
            self.connection
                .record_failure(crate::TransportError::Protocol(
                    "local destination relay rejected protocol state".to_owned(),
                ));
        }
        if let Some(dmabuf) = &self.dmabuf {
            for (_, imported) in self.buffers.drain() {
                dmabuf.remove_external(&imported.access);
            }
        } else if !self.buffers.is_empty() {
            self.connection
                .record_failure(crate::TransportError::Protocol(
                    "native buffer cache exists without a DMA-BUF context".to_owned(),
                ));
            self.buffers.clear();
        }
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

fn protocol_error(error: impl std::fmt::Display) -> weld_hoist_core::HoistPortError {
    Box::new(crate::TransportError::Protocol(error.to_string()))
}

#[cfg(test)]
mod tests {
    use weld_client::{
        ClientBufferId, ClientBufferMetadata, ClientBufferUseId, ClientCommitRevision, ClientId,
        ClientProvenance, ClientRequest, ClientSourceId, ClientSurfaceId, ClientSurfaceRequest,
        ClientSurfaceRequestKind, ClientSurfaceRole, Extent, SurfaceLayerId, ToplevelState,
        WindowDecoration, WireClientSurfaceCommit, WireClientSurfaceEventKind,
        WireSurfaceBufferChange, WireSurfaceBufferUpdate,
    };
    use weld_hoist_protocol::SourceEnvelope;

    use super::*;

    fn setup() -> (
        LocalPacketConnection,
        LocalPacketConnection,
        LocalDestinationPort,
        ClientSurfaceId,
        HoistSessionId,
    ) {
        let (source, destination) = LocalPacketConnection::pair().expect("transport pair");
        let upstream = ClientSourceId::new(0);
        let descriptor =
            ClientSourceDescriptor::new(ClientSourceId::new(1), ClientProvenance::Relocated);
        let surface = ClientSurfaceId::new(ClientId::new(upstream, 1), 2);
        let session = HoistSessionId::new(3);
        let port = LocalDestinationPort::new_without_dmabuf(destination.clone(), descriptor);
        (source, destination, port, surface, session)
    }

    #[test]
    fn structural_records_keep_source_identity_and_destination_messages_roundtrip() {
        let (source, destination, mut port, surface, session) = setup();
        let messages: [SourceMessage<LocalBuffer>; 3] = [
            SourceMessage::Mapped { surface },
            SourceMessage::Surface(WireClientSurfaceEvent {
                surface,
                kind: WireClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(
                    ToplevelState {
                        parent: None,
                        decoration: WindowDecoration::ClientSide,
                    },
                )),
            }),
            SourceMessage::BufferRetired {
                buffer: ClientBufferId::new(surface.source(), 8),
            },
        ];
        for message in messages {
            source
                .queue(&SourceEnvelope { session, message }, Vec::new())
                .expect("source record");
        }
        source.pump().expect("source send");

        let records = port.poll().expect("destination records");
        assert!(matches!(
            records[0],
            DestinationPortRecord {
                session: observed,
                event: DestinationPortEvent::MappedSurface(observed_surface),
            } if observed == session && observed_surface == surface
        ));
        assert!(matches!(
            &records[1],
            DestinationPortRecord {
                session: observed,
                event: DestinationPortEvent::Surface(event),
            } if *observed == session && event.surface == surface
        ));
        assert_eq!(records.len(), 2);

        let request = ClientRequest::Surface(ClientSurfaceRequest {
            surface,
            kind: ClientSurfaceRequestKind::Close,
        });
        port.submit(DestinationPortCommand::Message(DestinationEnvelope {
            session,
            message: DestinationMessage::Request(request.clone()),
        }))
        .expect("destination request");
        destination.pump().expect("destination send");
        let packets = source
            .drain::<DestinationEnvelope>()
            .expect("destination packet");
        assert!(matches!(
            &packets[0].message,
            DestinationEnvelope {
                session: observed,
                message: DestinationMessage::Request(observed_request),
            } if *observed == session && *observed_request == request
        ));
    }

    #[test]
    fn rejected_shm_replacement_returns_its_source_buffer_use() {
        let (source, destination, mut port, surface, session) = setup();
        let source_id = surface.source();
        let source_use = ClientBufferUseId::new(source_id, 5);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), false);
        let event = WireClientSurfaceEvent {
            surface,
            kind: WireClientSurfaceEventKind::Commit(WireClientSurfaceCommit {
                revision: ClientCommitRevision::new(1),
                alpha_mode: Default::default(),
                mapped: true,
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: vec![WireSurfaceBufferUpdate {
                    layer: SurfaceLayerId::new(1),
                    change: WireSurfaceBufferChange::Replaced {
                        metadata,
                        buffer: LocalBuffer {
                            buffer: ClientBufferId::new(source_id, 4),
                            use_id: source_use,
                            content: LocalBufferContent::Shm(crate::LocalShm {
                                descriptor_index: 0,
                            }),
                        },
                    },
                }],
            }),
        };
        source
            .queue(
                &SourceEnvelope {
                    session,
                    message: SourceMessage::Surface(event),
                },
                Vec::new(),
            )
            .expect("malformed source record");
        source.pump().expect("source send");

        assert!(port.poll().expect("rejected packet").is_empty());
        destination.pump().expect("release send");
        let packets = source
            .drain::<DestinationEnvelope>()
            .expect("buffer release");
        assert!(matches!(
            packets[0].message,
            DestinationEnvelope {
                session: observed,
                message: DestinationMessage::BufferReleased { use_id },
            } if observed == session && use_id == source_use
        ));
    }
}
