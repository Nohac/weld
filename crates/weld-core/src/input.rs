//! Backend-neutral input contracts and host input sources.

#[path = "input_raw.rs"]
mod raw;
#[path = "input_source/mod.rs"]
pub mod source;

pub use raw::*;

use crate::surface::{SurfaceId, SurfaceLayerId};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SurfaceHit {
    pub surface: SurfaceId,
    pub layer: SurfaceLayerId,
    pub local_position: InputPosition,
}
