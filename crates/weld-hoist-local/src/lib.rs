//! Linux-local hoist transport over Unix sequenced-packet sockets.
//!
//! Control records are encoded with Postcard. Native buffer file descriptors
//! travel beside the matching datagram through `SCM_RIGHTS`, preserving packet
//! and descriptor ownership without a byte-stream framing layer.

mod adapter;
mod native;
mod socket;
mod wire;

pub use adapter::{
    LocalDestinationEndpoint, local_destination_registration, local_source_registration,
};
pub use native::{
    ExportedLocalBuffer, ensure_descriptors_consumed, export_local_buffer, import_local_dmabuf,
    import_local_shm,
};
pub use socket::{
    LocalPacketConnection, LocalPacketListener, LocalPeerRole, ReceivedLocalPacket, TransportError,
};
pub use wire::{
    LocalBuffer, LocalBufferContent, LocalDestinationMessage, LocalDestinationPacket, LocalDmabuf,
    LocalDmabufPlane, LocalShm, LocalSourceMessage, LocalSourcePacket,
};
