//! Protocol-neutral client contracts used by the native host.

pub use weld_client::{
    ClientSurfaceId as SurfaceId, Extent, LogicalPoint, LogicalSize, PopupState as PopupDescriptor,
    SurfaceContentView, SurfaceInputPlacement, SurfaceInputRect, SurfaceLayerId,
    SurfaceLayerPlacement, SurfaceWindowGeometry,
    ToplevelInteractionRequestKind as WindowInteractionRequestKind, WindowDecoration,
    WindowResizeEdge,
};

/// Protocol-neutral request emitted by application policy for the native host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceAction {
    Close {
        surface: SurfaceId,
    },
    Focus {
        surface: Option<SurfaceId>,
    },
    Resize {
        surface: SurfaceId,
        logical_size: Extent,
    },
    SetOutputs {
        surface: SurfaceId,
        outputs: Vec<crate::OutputId>,
        preferred: Option<crate::OutputId>,
    },
}
