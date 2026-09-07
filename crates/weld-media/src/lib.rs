//! Transport- and platform-neutral media identities and payload contracts.
//!
//! Codec backends own native graphics handles, worker threads, and hardware
//! APIs. Transport bindings carry identities and payloads without learning how
//! a frame was encoded or decoded. Worker timing is local and never serialized.

mod frame;
mod id;
mod timing;

pub use frame::{EncodedAccessUnit, EncodedFrameKind, VideoCodec};
pub use id::{MediaFrameId, MediaStreamId, StreamGeneration};
pub use timing::{DecodePipelineTiming, DecodeTiming};
