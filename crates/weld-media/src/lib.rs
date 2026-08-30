//! Transport- and platform-neutral media identities and payload contracts.
//!
//! Codec backends own native graphics handles, worker threads, and hardware
//! APIs. Transport bindings carry these values without learning how a frame
//! was encoded or decoded.

mod frame;
mod id;

pub use frame::{EncodedAccessUnit, EncodedFrameKind, VideoCodec};
pub use id::{MediaFrameId, MediaStreamId, StreamGeneration};
