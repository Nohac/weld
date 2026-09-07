//! Transport-neutral records shared by Weld hoist endpoints.
//!
//! Bindings own framing, serialization, native handles, media payload storage,
//! queues, and runtime integration. This crate owns only the records whose
//! meaning must remain identical across those bindings.

use std::fmt;

use serde::{Deserialize, Serialize};
use weld_client::{
    ClientBufferId, ClientBufferUseId, ClientInputEvent, ClientRequest, ClientSurfaceId,
    WireClientInputEvent, WireClientSurfaceEvent,
};
use weld_media::{EncodedFrameKind, MediaFrameId, VideoCodec};

/// Sanity bound shared by encoded framing and payload admission.
pub const MAX_ENCODED_ACCESS_UNIT_BYTES: usize = 32 * 1024 * 1024;

/// Exact pre-1.0 protocol revision understood by this build.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolRevision(u32);

impl ProtocolRevision {
    pub const CURRENT: Self = Self(4);

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
    BufferRetired {
        buffer: ClientBufferId,
    },
    Withdraw {
        surface: ClientSurfaceId,
    },
    Ended,
    // Added after the original surface messages; retain its wire discriminant.
    Mapped {
        surface: ClientSurfaceId,
    },
    /// Lossless cursor feedback, independent of encoded surface frames.
    Cursor {
        update: weld_client::ClientCursorUpdate,
        sequence: u64,
    },
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
    /// Acknowledges receipt, not visibility or presentation of the cursor.
    CursorReceived {
        surface: ClientSurfaceId,
        sequence: u64,
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
            Self::CursorReceived { .. } => "cursor-received",
        }
    }
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
    fn cursor_feedback_roundtrips_losslessly_and_rejects_invalid_rasters() {
        let image = weld_client::ClientCursorImage::new(128, 128, (7, 11), vec![127; 65536])
            .expect("bounded image");
        let cursor = weld_client::ClientCursor::Image(image);
        let bytes = postcard::to_allocvec(&cursor).expect("serialize cursor");
        assert!(bytes.len() < 192 * 1024);
        assert_eq!(
            postcard::from_bytes::<weld_client::ClientCursor>(&bytes).expect("decode"),
            cursor
        );

        let small = weld_client::ClientCursor::Image(
            weld_client::ClientCursorImage::new(1, 1, (0, 0), vec![255; 4]).expect("small cursor"),
        );
        let mut bytes = postcard::to_allocvec(&small).expect("encode small");
        // Image discriminant, width, height, hotspot, then pixel data.
        bytes[1] = 0;
        assert!(postcard::from_bytes::<weld_client::ClientCursor>(&bytes).is_err());
        for cursor in [
            weld_client::ClientCursor::Hidden,
            weld_client::ClientCursor::Named(weld_client::CursorIcon::Text),
        ] {
            let bytes = postcard::to_allocvec(&cursor).expect("encode shape");
            assert_eq!(
                postcard::from_bytes::<weld_client::ClientCursor>(&bytes).expect("decode shape"),
                cursor
            );
        }
        assert!(postcard::from_bytes::<weld_client::ClientCursor>(&[0, 127]).is_err());
    }

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
