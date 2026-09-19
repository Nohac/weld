use serde::{Deserialize, Serialize};

#[cfg(test)]
mod metadata_tests {
    use weld_client::{
        ClientId, ClientSourceId, ClientSurfaceId, ClientSurfaceMetadata, WireClientSurfaceEvent,
        WireClientSurfaceEventKind,
    };

    #[test]
    fn surface_labels_roundtrip_and_oversized_wire_labels_are_rejected() {
        let surface = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1);
        let event = WireClientSurfaceEvent::<()> {
            surface,
            kind: WireClientSurfaceEventKind::Metadata(
                ClientSurfaceMetadata::new("test.app".into(), "Stereo window".into()).unwrap(),
            ),
        };
        let bytes = postcard::to_allocvec(&event).unwrap();
        let decoded: WireClientSurfaceEvent<()> = postcard::from_bytes(&bytes).unwrap();
        assert!(
            matches!(decoded.kind, WireClientSurfaceEventKind::Metadata(metadata) if metadata.title() == "Stereo window" && metadata.app_id() == "test.app")
        );
        // The metadata wire representation is a pair of strings. Verify the
        // constructor's invariant is also enforced by actual deserialization.
        let oversized = postcard::to_allocvec(&("test.app", "x".repeat(1025))).unwrap();
        assert!(postcard::from_bytes::<ClientSurfaceMetadata>(&oversized).is_err());
    }
}
use weld_hoist_protocol::{
    EncodedAccessUnitHeader, EncodedBuffer, MediaEnvelope, ProtocolRevision, SourceEnvelope,
    SourceMessage,
};

pub use weld_hoist_protocol::{
    DestinationEnvelope as LocalDestinationPacket, DestinationMessage as LocalDestinationMessage,
    SurfaceMode as LocalSurfaceMode,
};

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct LocalBootstrapOffer {
    pub revision: ProtocolRevision,
    pub mode: LocalSurfaceMode,
    pub media_descriptor: Option<u16>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct LocalBootstrapAcknowledgement {
    pub revision: ProtocolRevision,
    pub rejection: Option<String>,
}

/// One DMA-BUF plane whose descriptor is attached to the containing packet.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalDmabufPlane {
    pub descriptor_index: u16,
    pub offset: u32,
    pub stride: u32,
}

/// Linux-local native-buffer metadata carried by a surface replacement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalDmabuf {
    pub format: u32,
    pub modifier: u64,
    pub flags: u32,
    pub planes: Vec<LocalDmabufPlane>,
}

/// One sealed descriptor containing tightly packed BGRA pixels.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalShm {
    pub descriptor_index: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LocalBufferContent {
    ImportedDmabuf(LocalDmabuf),
    ReusedDmabuf,
    Shm(LocalShm),
    Encoded(EncodedBuffer),
}

/// One committed buffer use and its Unix-local content binding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalBuffer {
    pub buffer: weld_client::ClientBufferId,
    pub use_id: weld_client::ClientBufferUseId,
    pub content: LocalBufferContent,
}

pub type LocalSourcePacket = SourceEnvelope<LocalBuffer>;
pub type LocalSourceMessage = SourceMessage<LocalBuffer>;
pub type LocalEncodedSourcePacket = SourceEnvelope<EncodedBuffer>;
pub type LocalEncodedBuffer = EncodedBuffer;

/// Unix media binding for one transport-neutral access-unit header.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LocalEncodedAccessUnit {
    pub header: EncodedAccessUnitHeader,
    pub payload_descriptor: u16,
}

pub type LocalMediaPacket = MediaEnvelope<LocalEncodedAccessUnit>;
