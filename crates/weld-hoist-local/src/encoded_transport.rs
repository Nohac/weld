//! Unix seqpacket binding for the transport-neutral encoded hoist ports.

use tracing::warn;
use weld_hoist_core::{HoistPortError, HoistPortResult};
use weld_hoist_encoded::{
    EncodedDestinationTransport, EncodedSourceTransport, SourceTransportPacket,
};
use weld_hoist_protocol::DestinationEnvelope;

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
    fn send(&self, packet: SourceTransportPacket) -> HoistPortResult<()> {
        let result = match packet {
            SourceTransportPacket::Control(packet) => self.control.queue(&packet, Vec::new()),
            SourceTransportPacket::Media(packet) => {
                let (access_unit, descriptors) = export_access_unit(packet.access_unit)
                    .map_err(|error| TransportError::Protocol(error.to_string()))?;
                self.media.queue(
                    &LocalMediaPacket {
                        session: packet.session,
                        access_unit,
                    },
                    descriptors,
                )
            }
        };
        result.map_err(transport_error)
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
    pub(crate) fn new(control: LocalPacketConnection, media: LocalPacketConnection) -> Self {
        Self { control, media }
    }
}

impl EncodedDestinationTransport for LocalEncodedDestinationTransport {
    fn send(&self, packet: DestinationEnvelope) -> HoistPortResult<()> {
        self.control
            .queue(&packet, Vec::new())
            .map_err(transport_error)
    }

    fn drain(&self) -> HoistPortResult<Vec<SourceTransportPacket>> {
        let control = self
            .control
            .drain::<LocalEncodedSourcePacket>()
            .map_err(transport_error)?;
        let media = self
            .media
            .drain::<LocalMediaPacket>()
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

    #[test]
    fn unix_binding_translates_independent_control_and_media_packets() {
        let (source_control, destination_control) =
            LocalPacketConnection::pair().expect("control pair");
        let (source_media, destination_media) = LocalPacketConnection::pair().expect("media pair");
        let source = LocalEncodedSourceTransport::new(source_control.clone(), source_media.clone());
        let destination = LocalEncodedDestinationTransport::new(
            destination_control.clone(),
            destination_media.clone(),
        );
        let session = HoistSessionId::new(1);
        let surface = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 2), 3);
        let frame = MediaFrameId::new(MediaStreamId::new(4), StreamGeneration::new(5), 6);

        source
            .send(SourceTransportPacket::Media(MediaEnvelope {
                session,
                access_unit: EncodedAccessUnit {
                    frame,
                    codec: VideoCodec::Av1,
                    kind: EncodedFrameKind::Keyframe,
                    timestamp_micros: 7,
                    payload: vec![8, 9],
                },
            }))
            .expect("media");
        source
            .send(SourceTransportPacket::Control(SourceEnvelope {
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
            }))
            .expect("control");
        source_control.pump().expect("control pump");
        source_media.pump().expect("media pump");

        let packets = destination.drain().expect("destination packets");
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

        source.disconnect();
        assert!(source_control.is_disconnected());
        assert!(source_media.is_disconnected());
        destination.disconnect();
        assert!(destination_control.is_disconnected());
        assert!(destination_media.is_disconnected());
    }
}
