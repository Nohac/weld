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
    // Unreleased development baseline: rebuild peers together. This is not a
    // promise of schema compatibility between arbitrary development builds.
    pub const CURRENT: Self = Self(1);

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
    fn bitrate_preferences_roundtrip_and_reject_invalid_group_and_role() {
        use weld_client::{
            ClientSurfaceRequestKind, PresentationGroupId, PresentationRole,
            SurfaceBitratePreference,
        };
        for raw in [0_u32, 65_536, u32::MAX] {
            let bytes = postcard::to_allocvec(&raw).expect("raw id");
            assert!(postcard::from_bytes::<PresentationGroupId>(&bytes).is_err());
        }
        assert!(postcard::from_bytes::<PresentationRole>(&[3]).is_err());
        for preference in [
            None,
            Some(SurfaceBitratePreference {
                group: PresentationGroupId::try_from(65_535).expect("group"),
                role: PresentationRole::Primary,
            }),
        ] {
            let request = ClientSurfaceRequestKind::SetBitratePreference { preference };
            let bytes = postcard::to_allocvec(&request).expect("encode");
            assert_eq!(
                postcard::from_bytes::<ClientSurfaceRequestKind>(&bytes).expect("decode"),
                request
            );
        }
    }

    #[test]
    fn presentation_rate_is_validated_on_the_wire() {
        for raw in [0_u32, 999, 1_000_001, u32::MAX] {
            let bytes = postcard::to_allocvec(&raw).expect("encode raw rate");
            assert!(postcard::from_bytes::<weld_client::PresentationRate>(&bytes).is_err());
        }
        for rate in [
            None,
            Some(weld_client::PresentationRate::try_from(90_000).expect("rate")),
        ] {
            let request = weld_client::ClientSurfaceRequestKind::SetPresentation { rate };
            let bytes = postcard::to_allocvec(&request).expect("encode request");
            assert_eq!(
                postcard::from_bytes::<weld_client::ClientSurfaceRequestKind>(&bytes)
                    .expect("decode request"),
                request
            );
        }
    }

    #[test]
    fn key_press_repeat_release_roundtrip_in_order_without_state_collapse() {
        use weld_client::{
            ClientId, ClientInputTarget, ClientSourceId, InputEventKind, KeyboardKeyState,
            LinuxKeycode,
        };
        let surface = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 1), 1);
        let states = [
            KeyboardKeyState::Pressed,
            KeyboardKeyState::Repeated,
            KeyboardKeyState::Repeated,
            KeyboardKeyState::Released,
        ];
        let events: Vec<_> = states
            .into_iter()
            .enumerate()
            .map(|(time, state)| DestinationEnvelope {
                session: HoistSessionId::new(1),
                message: DestinationMessage::input(ClientInputEvent {
                    target: ClientInputTarget::Keyboard { surface },
                    host_position: None,
                    event: InputEventKind::Keyboard {
                        keycode: LinuxKeycode(30),
                        state,
                    },
                    time: time as u32,
                }),
            })
            .collect();
        let bytes = postcard::to_allocvec(&events).expect("encode input burst");
        let decoded: Vec<DestinationEnvelope> =
            postcard::from_bytes(&bytes).expect("decode input burst");
        for ((index, envelope), state) in decoded.into_iter().enumerate().zip(states) {
            let DestinationMessage::Input(input) = envelope.message else {
                panic!("input record");
            };
            let input = input.into_client_event();
            assert_eq!(
                input.event,
                InputEventKind::Keyboard {
                    keycode: LinuxKeycode(30),
                    state
                }
            );
            assert_eq!(input.time, index as u32);
        }
    }

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
        assert_eq!(ProtocolRevision::CURRENT.raw(), 1);
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
