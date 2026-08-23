//! Protocol-neutral client contracts used by the native host.

pub use weld_client::{
    ClientSurfaceId as SurfaceId, Extent, LogicalPoint, LogicalSize, PopupState as PopupDescriptor,
    SurfaceContentView, SurfaceInputPlacement, SurfaceInputRect, SurfaceLayerId,
    SurfaceLayerPlacement, SurfaceWindowGeometry,
    ToplevelInteractionRequestKind as WindowInteractionRequestKind, WindowDecoration,
    WindowResizeEdge,
};
