//! Transport-neutral encoded hoist scheduling and codec integration.

mod activity;
mod bitrate;
mod budget;
mod codec;
mod destination_observations;
#[cfg(feature = "native")]
pub mod native;
mod observations;
mod output;
mod scheduling;
mod state;
mod transport_observations;

pub use activity::SchedulingPolicy;
pub use bitrate::{
    BitrateRequest, EncoderBitrateLimits, EncoderRateApplication, EncoderRateControl,
    EncoderStreamStatus,
};
pub use budget::{
    BitrateAllocationPolicy, BitrateBudgetSnapshot, InsufficientBitrateBudget, SharedBitrateBudget,
};
pub use observations::TimingSummary;
pub use transport_observations::{
    MediaSendCounters, MediaSendSnapshot, NetworkPathSnapshot, TransportSnapshot,
};

pub use codec::{
    DecodeBackend, DecodeCompletion, DecodeRequest, DecodedFrame, DecodedFramePublisher,
    EncodeBackend, EncodeCompletion, EncodeInput, EncodeRequest, PreparedEncodeInput, SubmitError,
};
#[cfg(feature = "vaapi")]
pub use codec::{decode_backend, encode_backend};
pub use state::{
    EncodedDestinationPort, EncodedDestinationTransport, EncodedSourceOptions, EncodedSourcePort,
    EncodedSourceTransport, ReceiveBudget, SendStatus, SourceTransportPacket,
};
