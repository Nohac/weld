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
    PointerMotion {
        position: InputPosition,
        target: Option<SurfaceHit>,
    },
    PointerButton {
        position: InputPosition,
        target: Option<SurfaceHit>,
        button: LinuxButtonCode,
        state: ButtonState,
    },
    PointerAxis {
        axis: RawScrollFrame,
    },
    Keyboard {
        keycode: LinuxKeycode,
        state: ButtonState,
    },
    HostFocusLost,
}
