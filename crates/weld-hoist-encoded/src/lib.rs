//! Transport-neutral encoded hoist scheduling and codec integration.

mod codec;
mod state;

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
