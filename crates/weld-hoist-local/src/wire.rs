use serde::{Deserialize, Serialize};
use weld_client::{
    ClientBufferId, ClientBufferUseId, ClientInputEvent, ClientRequest, ClientSurfaceId,
    WireClientInputEvent, WireClientSurfaceEvent,
};
use weld_hoist_core::HoistSessionId;

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
    pub buffer: ClientBufferId,
    pub use_id: ClientBufferUseId,
    pub format: u32,
    pub modifier: u64,
    pub flags: u32,
    pub planes: Vec<LocalDmabufPlane>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum LocalBuffer {
    Imported(LocalDmabuf),
    Reused {
        buffer: ClientBufferId,
        use_id: ClientBufferUseId,
    },
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
    BufferReleased { use_id: ClientBufferUseId },
    Reclaim,
}

impl LocalDestinationMessage {
    pub fn input(event: ClientInputEvent) -> Self {
        Self::Input(event.into())
    }
}
