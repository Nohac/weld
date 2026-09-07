//! Unix seqpacket binding for the transport-neutral encoded hoist ports.

use tracing::warn;
use weld_hoist_core::{HoistPortError, HoistPortResult};
use weld_hoist_encoded::{
    EncodedDestinationTransport, EncodedSourceTransport, ReceiveBudget, SendStatus,
    SourceTransportPacket,
};
use weld_hoist_protocol::{DestinationEnvelope, MAX_ENCODED_ACCESS_UNIT_BYTES};

use crate::{
    LocalDestinationPacket, LocalEncodedSourcePacket, LocalMediaPacket, LocalPacketConnection,
    TransportError, import_access_unit, media::export_access_unit,
};

pub(crate) struct LocalEncodedSourceTransport {
    control: LocalPacketConnection,
    media: LocalPacketConnection,
}

impl LocalEncodedSourceTransport {
    pub(crate) fn new(control: LocalPacketConnection, media: LocalPacketConnection) -> Self {
        Self { control, media }
    }
}

impl EncodedSourceTransport for LocalEncodedSourceTransport {
    fn try_send(
        &self,
        packet: SourceTransportPacket,
    ) -> HoistPortResult<SendStatus<SourceTransportPacket>> {
        if let SourceTransportPacket::Media(media) = &packet
            && !self
                .media
                .can_queue(1, media.access_unit.payload.len().saturating_add(1024))
        {
            return Ok(SendStatus::Busy(packet));
        }
        let result = match &packet {
            SourceTransportPacket::Control(packet) => self.control.try_queue(packet, Vec::new(), 0),
            SourceTransportPacket::Media(packet) => {
                let (access_unit, descriptors) = export_access_unit(&packet.access_unit)
                    .map_err(|error| TransportError::Protocol(error.to_string()))?;
                self.media.try_queue(
                    &LocalMediaPacket {
                        session: packet.session,
                        access_unit,
                    },
                    descriptors,
                    packet.access_unit.payload.len(),
                )
            }
        };
        match result {
            Ok(()) => Ok(SendStatus::Sent),
            Err(TransportError::SendQueueFull { .. }) => Ok(SendStatus::Busy(packet)),
            Err(error) => Err(transport_error(error)),
        }
    }

    fn media_headroom(&self) -> bool {
        let (records, bytes) = self.media.send_backlog();
        records < 4 && bytes < 8 * 1024 * 1024
    }

    fn drain(&self) -> HoistPortResult<Vec<DestinationEnvelope>> {
        let packets = self
            .control
            .drain::<LocalDestinationPacket>()
            .map_err(transport_error)?;
        packets
            .into_iter()
            .map(|packet| {
                if packet.file_descriptors.is_empty() {
                    Ok(packet.message)
                } else {
                    Err(protocol_error(
                        "destination control packet attached descriptors",
                    ))
                }
            })
            .collect()
    }

    fn disconnect(&self) {
        record_disconnect(
            &self.control,
            "encoded source relay rejected protocol state",
        );
        record_disconnect(&self.media, "encoded source relay rejected protocol state");
    }
}

pub(crate) struct LocalEncodedDestinationTransport {
    control: LocalPacketConnection,
    media: LocalPacketConnection,
}

impl LocalEncodedDestinationTransport {
    pub(crate) fn new(
        control: LocalPacketConnection,
        media: LocalPacketConnection,
    ) -> Result<Self, TransportError> {
        // At most two sealed payload descriptors wait outside decoder admission.
        media.set_receive_limit(2)?;
        Ok(Self { control, media })
    }
}

impl EncodedDestinationTransport for LocalEncodedDestinationTransport {
    fn send(&self, packet: DestinationEnvelope) -> HoistPortResult<()> {
        self.control
            .queue(&packet, Vec::new())
            .map_err(transport_error)
    }

    fn drain(&self, budget: ReceiveBudget) -> HoistPortResult<Vec<SourceTransportPacket>> {
        let control = self
            .control
            .drain_limited::<LocalEncodedSourcePacket>(budget.control_records)
            .map_err(transport_error)?;
        let media = self
            .media
            .drain_limited::<LocalMediaPacket>(
                budget
                    .media_records
                    .min(budget.media_bytes / MAX_ENCODED_ACCESS_UNIT_BYTES),
            )
            .map_err(transport_error)?;
        let mut packets = Vec::with_capacity(control.len() + media.len());
        for packet in control {
            if !packet.file_descriptors.is_empty() {
                return Err(protocol_error(
                    "encoded control packet attached descriptors",
                ));
            }
            packets.push(SourceTransportPacket::Control(packet.message));
        }
        for packet in media {
            let access_unit =
                import_access_unit(packet.message.access_unit, packet.file_descriptors)
                    .map_err(protocol_error)?;
            packets.push(SourceTransportPacket::Media(
                weld_hoist_protocol::MediaEnvelope {
                    session: packet.message.session,
                    access_unit,
                },
            ));
        }
        Ok(packets)
    }

    fn wake_if_readable(&self, budget: ReceiveBudget) -> HoistPortResult<()> {
        if budget.control_records > 0 {
            self.control.wake_buffered().map_err(transport_error)?;
        }
        if budget.media_records > 0 && budget.media_bytes >= MAX_ENCODED_ACCESS_UNIT_BYTES {
            self.media.wake_buffered().map_err(transport_error)?;
        }
        Ok(())
    }

    fn disconnect(&self) {
        record_disconnect(
            &self.control,
            "encoded destination relay rejected protocol state",
        );
        record_disconnect(
            &self.media,
            "encoded destination relay rejected protocol state",
        );
    }
}

fn record_disconnect(connection: &LocalPacketConnection, message: &str) {
    if !connection.is_disconnected() {
        connection.record_failure(TransportError::Protocol(message.to_owned()));
    }
}

fn transport_error(error: TransportError) -> HoistPortError {
    warn!(%error, "encoded local hoist transport failed");
    Box::new(error)
}

fn protocol_error(error: impl std::fmt::Display) -> HoistPortError {
    Box::new(TransportError::Protocol(error.to_string()))
}

#[cfg(test)]
mod tests {
    use weld_client::{ClientId, ClientSourceId, ClientSurfaceId};
    use weld_hoist_encoded::{EncodedDestinationTransport, EncodedSourceTransport};
    use weld_hoist_protocol::{
        DestinationMessage, EncodedBuffer, HoistSessionId, MediaEnvelope, SourceEnvelope,
        SourceMessage,
    };
    use weld_media::{
        EncodedAccessUnit, EncodedFrameKind, MediaFrameId, MediaStreamId, StreamGeneration,
        VideoCodec,
    };

    use super::*;

    fn send_source(
        source: &impl EncodedSourceTransport,
        packet: SourceTransportPacket,
    ) -> weld_hoist_core::HoistPortResult<()> {
        assert!(matches!(
            source.try_send(packet)?,
            weld_hoist_encoded::SendStatus::Sent
        ));
        Ok(())
    }

    #[test]
    fn unix_binding_translates_independent_control_and_media_packets() {
        let (source_control, destination_control) =
            LocalPacketConnection::pair().expect("control pair");
        let (source_media, destination_media) = LocalPacketConnection::pair().expect("media pair");
        let source = LocalEncodedSourceTransport::new(source_control.clone(), source_media.clone());
        let destination = LocalEncodedDestinationTransport::new(
            destination_control.clone(),
            destination_media.clone(),
        )
        .expect("destination transport");
        let session = HoistSessionId::new(1);
        let surface = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 2), 3);
        let frame = MediaFrameId::new(MediaStreamId::new(4), StreamGeneration::new(5), 6);

        send_source(
            &source,
            SourceTransportPacket::Media(MediaEnvelope {
                session,
                access_unit: EncodedAccessUnit {
                    frame,
                    codec: VideoCodec::Av1,
                    kind: EncodedFrameKind::Keyframe,
                    timestamp_micros: 7,
                    payload: vec![8, 9],
                },
            }),
        )
        .expect("media");
        send_source(
            &source,
            SourceTransportPacket::Control(SourceEnvelope {
                session,
                message: SourceMessage::Surface(weld_client::WireClientSurfaceEvent {
                    surface,
                    kind: weld_client::WireClientSurfaceEventKind::Commit(
                        weld_client::WireClientSurfaceCommit {
                            revision: weld_client::ClientCommitRevision::new(10),
                            alpha_mode: weld_client::SurfaceAlphaMode::Discarded,
                            mapped: true,
                            root: None,
                            window_geometry: None,
                            overlays: Vec::new(),
                            inputs: Vec::new(),
                            buffers: vec![weld_client::WireSurfaceBufferUpdate {
                                layer: weld_client::SurfaceLayerId::new(11),
                                change: weld_client::WireSurfaceBufferChange::Replaced {
                                    metadata: weld_client::ClientBufferMetadata::new(
                                        weld_client::Extent::new(1, 1),
                                        true,
                                    ),
                                    buffer: EncodedBuffer { frame },
                                },
                            }],
                        },
                    ),
                }),
            }),
        )
        .expect("control");
        source_control.pump().expect("control pump");
        source_media.pump().expect("media pump");

        let packets = destination
            .drain(ReceiveBudget::ALL)
            .expect("destination packets");
        assert_eq!(packets.len(), 2);
        assert!(
            matches!(&packets[0], SourceTransportPacket::Control(SourceEnvelope {
            message: SourceMessage::Surface(weld_client::WireClientSurfaceEvent {
                kind: weld_client::WireClientSurfaceEventKind::Commit(commit), ..
            }), ..
        }) if commit.alpha_mode == weld_client::SurfaceAlphaMode::Discarded)
        );
        assert!(matches!(packets[1], SourceTransportPacket::Media(_)));

        destination
            .send(weld_hoist_protocol::DestinationEnvelope {
                session,
                message: DestinationMessage::Reclaim,
            })
            .expect("destination reply");
        destination_control.pump().expect("destination pump");
        let replies = source.drain().expect("source replies");
        assert!(matches!(replies[0].message, DestinationMessage::Reclaim));

        send_source(
            &source,
            SourceTransportPacket::Control(SourceEnvelope {
                session,
                message: SourceMessage::Cursor {
                    update: weld_client::ClientCursorUpdate {
                        surface,
                        cursor: weld_client::ClientCursor::Named(weld_client::CursorIcon::Text),
                    },
                    sequence: 1,
                },
            }),
        )
        .expect("cursor");
        source_control.pump().expect("cursor pump");
        assert!(matches!(
            destination
                .drain(ReceiveBudget::ALL)
                .expect("cursor delivery")
                .as_slice(),
            [SourceTransportPacket::Control(SourceEnvelope {
                message: SourceMessage::Cursor { sequence: 1, .. },
                ..
            })]
        ));
        destination
            .send(weld_hoist_protocol::DestinationEnvelope {
                session,
                message: DestinationMessage::CursorReceived {
                    surface,
                    sequence: 1,
                },
            })
            .expect("cursor ack");
        destination_control.pump().expect("ack pump");
        assert!(matches!(
            source.drain().expect("ack delivery")[0].message,
            DestinationMessage::CursorReceived { sequence: 1, .. }
        ));

        source.disconnect();
        assert!(source_control.is_disconnected());
        assert!(source_media.is_disconnected());
        destination.disconnect();
        assert!(destination_control.is_disconnected());
        assert!(destination_media.is_disconnected());
    }
}
