//! Transport- and platform-neutral media identities and payload contracts.
//!
//! Codec backends own native graphics handles and hardware APIs. The optional
//! `decode` feature provides bounded worker execution without a native backend.
//! Transport bindings carry identities and payloads without learning how a frame
//! was encoded or decoded. Worker timing is local and never serialized.

#[cfg(feature = "decode")]
pub mod decode;
#[cfg(feature = "decode")]
mod submit;

#[cfg(feature = "decode")]
pub use submit::WorkerSubmitError;

#[cfg(feature = "config")]
mod config;
mod frame;
#[cfg(feature = "config")]
pub use config::DecoderConfig;
mod id;
mod timing;

pub use frame::{EncodedAccessUnit, EncodedFrameKind, VideoCodec};
pub use id::{MediaFrameId, MediaStreamId, StreamGeneration};
pub use timing::{DecodePipelineTiming, DecodeTiming};
