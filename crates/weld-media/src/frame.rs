use crate::MediaFrameId;

/// Codec carried by one negotiated stream generation.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoCodec {
    Av1,
    Vp9,
    H264,
}

/// Decoder recovery role of an encoded access unit.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodedFrameKind {
    Keyframe,
    Delta,
}

/// Independently owned compressed frame payload.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedAccessUnit {
    pub frame: MediaFrameId,
    pub codec: VideoCodec,
    pub kind: EncodedFrameKind,
    pub timestamp_micros: u64,
    pub payload: Vec<u8>,
}
