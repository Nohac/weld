//! Adapter-addressed input and retained device-paced routes.

use crate::{ClientSurfaceId, SurfaceLayerId};

/// Press or release state shared by all input adapters.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ButtonState {
    Pressed,
    Released,
}

/// A position in Weld compositor or client-local coordinates.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InputPosition {
    pub x: f64,
    pub y: f64,
}

impl InputPosition {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InputDelta {
    pub x: f64,
    pub y: f64,
}

impl InputDelta {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LinuxKeycode(pub u32);

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LinuxButtonCode(pub u32);

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawScrollSource {
    Wheel,
    Finger,
    Continuous,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawScrollPhase {
    Started,
    Moved,
    Ended,
    Cancelled,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RawScrollFrame {
    pub source: RawScrollSource,
    pub phase: RawScrollPhase,
    pub horizontal: f64,
    pub vertical: f64,
    pub horizontal_v120: Option<i32>,
    pub vertical_v120: Option<i32>,
    pub horizontal_stop: bool,
    pub vertical_stop: bool,
}

impl RawScrollFrame {
    pub const fn cancelled_finger(horizontal_stop: bool, vertical_stop: bool) -> Self {
        Self {
            source: RawScrollSource::Finger,
            phase: RawScrollPhase::Cancelled,
            horizontal: 0.0,
            vertical: 0.0,
            horizontal_v120: None,
            vertical_v120: None,
            horizontal_stop,
            vertical_stop,
        }
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerGesture {
    Swipe(TouchpadSwipe),
    Pinch(TouchpadPinch),
    Hold(TouchpadHold),
}

impl PointerGesture {
    pub const fn kind(self) -> PointerGestureKind {
        match self {
            Self::Swipe(_) => PointerGestureKind::Swipe,
            Self::Pinch(_) => PointerGestureKind::Pinch,
            Self::Hold(_) => PointerGestureKind::Hold,
        }
    }

    pub const fn is_begin(self) -> bool {
        matches!(
            self,
            Self::Swipe(TouchpadSwipe::Begin { .. })
                | Self::Pinch(TouchpadPinch::Begin { .. })
                | Self::Hold(TouchpadHold::Begin { .. })
        )
    }

    pub const fn is_end(self) -> bool {
        matches!(
            self,
            Self::Swipe(TouchpadSwipe::End { .. })
                | Self::Pinch(TouchpadPinch::End { .. })
                | Self::Hold(TouchpadHold::End { .. })
        )
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerGestureKind {
    Swipe,
    Pinch,
    Hold,
}

impl PointerGestureKind {
    pub const fn cancelled(self) -> PointerGesture {
        match self {
            Self::Swipe => PointerGesture::Swipe(TouchpadSwipe::End { cancelled: true }),
            Self::Pinch => PointerGesture::Pinch(TouchpadPinch::End { cancelled: true }),
            Self::Hold => PointerGesture::Hold(TouchpadHold::End { cancelled: true }),
        }
    }
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TouchpadSwipe {
    Begin { fingers: u32 },
    Update { delta: InputDelta },
    End { cancelled: bool },
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TouchpadPinch {
    Begin {
        fingers: u32,
    },
    Update {
        delta: InputDelta,
        scale: f64,
        rotation: f64,
    },
    End {
        cancelled: bool,
    },
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TouchpadHold {
    Begin { fingers: u32 },
    End { cancelled: bool },
}

/// Affine compositor-logical to client-layer-logical mapping.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
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

/// Frame-published pointer route retained by [`crate::ClientRuntime`].
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClientPointerRoute {
    pub surface: ClientSurfaceId,
    pub layer: SurfaceLayerId,
    pub transform: InputTransform,
}

/// Keyboard route selected independently from pointer hover.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientKeyboardRoute {
    pub surface: ClientSurfaceId,
}

/// Frame-published pointer target reconciled at an application boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClientPointerRouteUpdate {
    pub route: Option<ClientPointerRoute>,
    pub position: InputPosition,
    pub time: u32,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientInputTarget {
    Pointer {
        surface: ClientSurfaceId,
        layer: SurfaceLayerId,
    },
    Keyboard {
        surface: ClientSurfaceId,
    },
}

impl ClientInputTarget {
    pub const fn surface(self) -> ClientSurfaceId {
        match self {
            Self::Pointer { surface, .. } | Self::Keyboard { surface } => surface,
        }
    }
}

/// Input already addressed and transformed for one client adapter.
///
/// Pointer coordinates in [`Self::event`] are client-layer local. The same
/// [`InputEventKind`] inside [`RuntimeInputEvent`] carries compositor-global
/// coordinates until [`crate::ClientRuntime`] applies its retained route.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientInputEvent {
    pub target: ClientInputTarget,
    /// Compositor-global pointer position before route transformation.
    pub host_position: Option<InputPosition>,
    pub event: InputEventKind,
    pub time: u32,
}

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, PartialEq)]
pub enum InputEventKind {
    PointerMotion {
        position: InputPosition,
    },
    PointerLeft {
        position: InputPosition,
    },
    PointerButton {
        position: Option<InputPosition>,
        button: LinuxButtonCode,
        state: ButtonState,
    },
    PointerAxis {
        position: Option<InputPosition>,
        axis: RawScrollFrame,
    },
    PointerGesture {
        gesture: PointerGesture,
    },
    Keyboard {
        keycode: LinuxKeycode,
        state: ButtonState,
    },
}

/// Unconsumed host input entering the device-paced client runtime.
///
/// Pointer coordinates in [`RuntimeInputEventKind::Input`] are compositor
/// global. ClientRuntime transforms them before constructing a
/// [`ClientInputEvent`].
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeInputEvent {
    pub event: RuntimeInputEventKind,
    pub time: u32,
}

impl RuntimeInputEvent {
    pub const fn new(event: RuntimeInputEventKind, time: u32) -> Self {
        Self { event, time }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RuntimeInputEventKind {
    Input(InputEventKind),
    HostFocusLost,
}
