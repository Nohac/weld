//! Linux-local hoist transport over Unix sequenced-packet sockets.
//!
//! Control records are encoded with Postcard. Native buffer file descriptors
//! travel beside the matching datagram through `SCM_RIGHTS`, preserving packet
//! and descriptor ownership without a byte-stream framing layer.

mod adapter;
mod bootstrap;
mod destination;
mod encoded_transport;
mod media;
mod native;
mod socket;
mod source;
mod wire;

#[cfg(feature = "encoded-vaapi")]
pub use adapter::{
    EncodedSourceRegistrationOptions, encoded_destination_registration, encoded_source_registration,
};
pub use adapter::{
    LocalDestinationEndpoint, encoded_destination_registration_with_backend,
    encoded_source_registration_with_backend, local_destination_registration,
    local_source_registration,
};
pub use bootstrap::{LocalTransportConnections, bootstrap_destination, bootstrap_source};
pub(crate) use media::import_access_unit;
pub use native::{
    ExportedLocalBuffer, ensure_descriptors_consumed, export_local_buffer, import_local_dmabuf,
    import_local_shm,
};
pub use socket::{
    LocalPacketConnection, LocalPacketListener, LocalPeerRole, ReceivedLocalPacket, TransportError,
};
pub use weld_hoist_encoded::{
    DecodeBackend as LocalDecodeBackend, DecodeCompletion as LocalDecodeCompletion,
    DecodeRequest as LocalDecodeRequest, DecodedFrame as LocalDecodedFrame,
    EncodeBackend as LocalEncodeBackend, EncodeCompletion as LocalEncodeCompletion,
    EncodeInput as LocalEncodeInput, EncodeRequest as LocalEncodeRequest,
    SubmitError as LocalSubmitError,
};
pub(crate) use wire::{LocalBootstrapAcknowledgement, LocalBootstrapOffer};
pub use wire::{
    LocalBuffer, LocalBufferContent, LocalDestinationMessage, LocalDestinationPacket, LocalDmabuf,
    LocalDmabufPlane, LocalEncodedAccessUnit, LocalEncodedBuffer, LocalEncodedCommitOutcome,
    LocalEncodedSourcePacket, LocalMediaPacket, LocalShm, LocalSourceMessage, LocalSourcePacket,
    LocalSurfaceMode,
};
