//! Linux-local hoist transport over Unix sequenced-packet sockets.
//!
//! Control records are encoded with Postcard. Native buffer file descriptors
//! travel beside the matching datagram through `SCM_RIGHTS`, preserving packet
//! and descriptor ownership without a byte-stream framing layer.

mod socket;
mod wire;

pub use socket::{
    LocalPacketConnection, LocalPacketListener, LocalPeerRole, ReceivedLocalPacket, TransportError,
};
pub use wire::{
    LocalDestinationMessage, LocalDestinationPacket, LocalDmabuf, LocalDmabufPlane,
    LocalSourceMessage, LocalSourcePacket,
};
