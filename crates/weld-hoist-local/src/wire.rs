use serde::{Deserialize, Serialize};
use weld_client::{
    ClientBufferId, ClientBufferUseId, ClientCommitRevision, ClientInputEvent, ClientRequest,
    ClientSurfaceId, WireClientInputEvent, WireClientSurfaceEvent,
};
use weld_hoist_core::HoistSessionId;
use weld_media::{EncodedFrameKind, MediaFrameId, VideoCodec};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LocalSurfaceMode {
    Native,
    EncodedOpaque(VideoCodec),
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct LocalBootstrapOffer {
    pub mode: LocalSurfaceMode,
    pub media_descriptor: Option<u16>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct LocalBootstrapAcknowledgement {
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
    Encoded(LocalEncodedBuffer),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalEncodedBuffer {
    pub frame: MediaFrameId,
}

/// One committed buffer use and its transport-specific content binding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalBuffer {
    pub buffer: ClientBufferId,
    pub use_id: ClientBufferUseId,
    pub content: LocalBufferContent,
}

/// One source-to-destination packet. The session is common to every message.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LocalSourcePacket {
    pub session: HoistSessionId,
    pub message: LocalSourceMessage,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum LocalSourceMessage {
    Surface(WireClientSurfaceEvent<LocalBuffer>),
    BufferRetired { buffer: ClientBufferId },
    Withdraw { surface: ClientSurfaceId },
    Ended,
}

/// One destination-to-source packet. The session is common to every message.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LocalDestinationPacket {
    pub session: HoistSessionId,
    pub message: LocalDestinationMessage,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum LocalDestinationMessage {
    Request(ClientRequest),
    Input(WireClientInputEvent),
    BufferReleased {
        use_id: ClientBufferUseId,
    },
    Reclaim,
    EncodedCommitFinished {
        surface: ClientSurfaceId,
        revision: ClientCommitRevision,
        outcome: LocalEncodedCommitOutcome,
    },
}

impl LocalDestinationMessage {
    pub(crate) const fn kind(&self) -> &'static str {
        match self {
            Self::Request(_) => "request",
            Self::Input(_) => "input",
            Self::BufferReleased { .. } => "buffer-released",
            Self::Reclaim => "reclaim",
            Self::EncodedCommitFinished { .. } => "encoded-commit-finished",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LocalEncodedCommitOutcome {
    Applied,
    Dropped,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LocalEncodedAccessUnit {
    pub frame: MediaFrameId,
    pub codec: VideoCodec,
    pub kind: EncodedFrameKind,
    pub timestamp_micros: u64,
    pub payload_descriptor: u16,
    pub payload_bytes: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LocalMediaPacket {
    pub session: HoistSessionId,
    pub access_unit: LocalEncodedAccessUnit,
}

impl LocalDestinationMessage {
    pub fn input(event: ClientInputEvent) -> Self {
        Self::Input(event.into())
    }
}
