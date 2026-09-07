use serde::{Deserialize, Serialize};
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
