//! Backend-neutral input contracts and host input sources.

#[path = "input_raw.rs"]
mod raw;
#[path = "input_source/mod.rs"]
pub mod source;

pub use raw::*;

use crate::surface::{SurfaceId, SurfaceLayerId};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceHit {
    pub surface: SurfaceId,
    pub layer: SurfaceLayerId,
    pub local_position: InputPosition,
}

/// Frame-published mapping from compositor coordinates into one client input
/// layer. Core retains this mapping between Bevy updates so it can deliver raw
/// pointer input at device pace without re-running application policy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceInputTarget {
    pub surface: SurfaceId,
    pub layer: SurfaceLayerId,
    pub transform: InputTransform,
}

impl SurfaceInputTarget {
    pub fn hit(self, position: InputPosition) -> SurfaceHit {
        SurfaceHit {
            surface: self.surface,
            layer: self.layer,
            local_position: self.transform.transform(position),
        }
    }
}

/// Affine compositor-logical to client-layer-logical coordinate mapping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputTransform {
    pub xx: f64,
    pub xy: f64,
    pub yx: f64,
    pub yy: f64,
    pub x: f64,
    pub y: f64,
}

impl InputTransform {
    pub const IDENTITY: Self = Self {
        xx: 1.0,
        xy: 0.0,
        yx: 0.0,
        yy: 1.0,
        x: 0.0,
        y: 0.0,
    };

    pub const fn transform(self, position: InputPosition) -> InputPosition {
        InputPosition::new(
            self.xx * position.x + self.xy * position.y + self.x,
            self.yx * position.x + self.yy * position.y + self.y,
        )
    }
}

/// Owned application-policy result consumed and validated by the host.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeatInputEffect {
    pub event: SeatInputEffectKind,
    pub time: u32,
}

impl SeatInputEffect {
    pub const fn new(event: SeatInputEffectKind, time: u32) -> Self {
        Self { event, time }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SeatInputEffectKind {
    PointerFocus {
        position: InputPosition,
        target: Option<SurfaceInputTarget>,
    },
}
