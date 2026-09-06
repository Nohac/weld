//! Transport-neutral encoded hoist scheduling and codec integration.

mod bitrate;
mod codec;
mod destination_observations;
mod observations;
mod state;
mod transport_observations;

pub use bitrate::{
    BitrateRequest, EncoderBitrateLimits, EncoderRateApplication, EncoderRateControl,
    EncoderStreamStatus,
};
pub use observations::TimingSummary;
pub use transport_observations::{
    MediaSendCounters, MediaSendSnapshot, NetworkPathSnapshot, TransportSnapshot,
};

pub use codec::{
    DecodeBackend, DecodeCompletion, DecodeRequest, DecodedFrame, EncodeBackend, EncodeCompletion,
    EncodeInput, EncodeRequest, SubmitError,
};
#[cfg(feature = "vaapi")]
pub use codec::{decode_backend, encode_backend};
pub use state::{
    EncodedDestinationPort, EncodedDestinationTransport, EncodedSourcePort, EncodedSourceTransport,
    SourceTransportPacket,
};
