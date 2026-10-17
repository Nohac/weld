//! Transport-neutral encoded hoist scheduling and codec integration.

mod activity;
mod adapter;
mod pending_source;
pub mod source_sessions;
pub use adapter::{
    DestinationEndpoint, SourceOptions, destination_registration_with_backend,
    source_registration_with_backend,
};
pub use pending_source::pending_source_registration_with_backend;

/// A live encoded source connection used by a registered relay endpoint.
pub trait SourceConnection:
    EncodedSourceTransport + weld_hoist_core::session::pairing::ConnectionControl + Clone + 'static
{
}

/// Admission advances during host servicing; dropping it cancels the pending connection.
pub trait PendingSource: 'static {
    type Transport: SourceConnection;
    fn poll(&mut self) -> anyhow::Result<Option<Self::Transport>>;
}
#[cfg(all(feature = "android", target_os = "android"))]
pub mod android;
mod bitrate;
mod budget;
mod codec;
mod destination_observations;
#[cfg(feature = "native")]
pub mod native;
mod observations;
mod output;
mod pacing;
mod scheduling;
mod state;
mod transport_observations;
use weld_hoist_core::{view, view_input};

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
