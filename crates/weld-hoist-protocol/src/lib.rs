//! Transport-neutral records shared by Weld hoist endpoints.
//!
//! Bindings own framing, serialization, native handles, media payload storage,
//! queues, and runtime integration. This crate owns only the records whose
//! meaning must remain identical across those bindings.

use std::fmt;

use serde::{Deserialize, Serialize};
use weld_client::{
    ClientBufferId, ClientBufferUseId, ClientCommitRevision, ClientInputEvent, ClientRequest,
    ClientSurfaceId, WireClientInputEvent, WireClientSurfaceEvent,
};
use weld_media::{EncodedFrameKind, MediaFrameId, VideoCodec};

/// Exact pre-1.0 protocol revision understood by this build.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolRevision(u32);

impl ProtocolRevision {
    pub const CURRENT: Self = Self(2);

    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    pub const fn ensure_compatible(self, peer: Self) -> Result<(), ProtocolRevisionMismatch> {
        if self.0 == peer.0 {
            Ok(())
        } else {
            Err(ProtocolRevisionMismatch { local: self, peer })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolRevisionMismatch {
    pub local: ProtocolRevision,
    pub peer: ProtocolRevision,
}

impl fmt::Display for ProtocolRevisionMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "hoist protocol revision {} does not match peer revision {}",
            self.local.raw(),
            self.peer.raw()
        )
    }
}

impl std::error::Error for ProtocolRevisionMismatch {}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct HoistSessionId(u64);

impl HoistSessionId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SurfaceMode {
    Native,
    EncodedOpaque(VideoCodec),
}

/// Source-to-destination semantic envelope.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SourceEnvelope<B> {
    pub session: HoistSessionId,
    pub message: SourceMessage<B>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum SourceMessage<B> {
    Surface(WireClientSurfaceEvent<B>),
    BufferRetired { buffer: ClientBufferId },
    Withdraw { surface: ClientSurfaceId },
    Ended,
    // Added after the original surface messages; retain its wire discriminant.
    Mapped { surface: ClientSurfaceId },
}

/// Destination-to-source semantic envelope.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DestinationEnvelope {
    pub session: HoistSessionId,
    pub message: DestinationMessage,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum DestinationMessage {
    Request(ClientRequest),
    Input(WireClientInputEvent),
    BufferReleased {
        use_id: ClientBufferUseId,
    },
    Reclaim,
    EncodedCommitFinished {
        surface: ClientSurfaceId,
        revision: ClientCommitRevision,
        outcome: EncodedCommitOutcome,
    },
}

impl DestinationMessage {
    pub fn input(event: ClientInputEvent) -> Self {
        Self::Input(event.into())
    }

    pub const fn kind(&self) -> &'static str {
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
pub enum EncodedCommitOutcome {
    Applied,
    Dropped,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EncodedBuffer {
    pub frame: MediaFrameId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EncodedAccessUnitHeader {
    pub frame: MediaFrameId,
    pub codec: VideoCodec,
    pub kind: EncodedFrameKind,
    pub timestamp_micros: u64,
    pub payload_bytes: u32,
}

/// Media-flow envelope whose payload binding is selected by the transport.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MediaEnvelope<A> {
    pub session: HoistSessionId,
    pub access_unit: A,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_revisions_require_an_exact_match() {
        assert!(
            ProtocolRevision::CURRENT
                .ensure_compatible(ProtocolRevision::CURRENT)
                .is_ok()
        );
        assert!(
            ProtocolRevision::CURRENT
                .ensure_compatible(ProtocolRevision::new(ProtocolRevision::CURRENT.raw() + 1))
                .is_err()
        );
    }
}
