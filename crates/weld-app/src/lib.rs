//! Bevy application integration and plugin-facing compositor model.

mod builder;
pub mod composition;
pub mod debug;
mod dmabuf;
pub mod input;
pub mod layer;
pub mod output;
mod shell;
#[path = "surface.rs"]
mod surface_impl;

/// Plugin-facing application surface model.
pub mod surface {
    pub use crate::surface_impl::{
        AppPopup, AppWindow, ClientDecorated, ClientSurface, MappedSurface, ServerDecorated,
        SurfaceAction, SurfaceActionQueue, SurfaceId, SurfaceLayerId, SurfaceNode,
        SurfaceSnapshotRevision, SurfaceSystems, SurfaceView, WindowDecoration,
        WindowInteractionRequest, WindowInteractionRequestKind, WindowResizeEdge,
    };

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub use crate::surface_impl::{
        HostSurfaceEvent, HostSurfaceEventKind, SurfaceBufferContent, SurfaceBufferUpdate,
        SurfaceContentView, SurfaceImageEncoding, SurfaceInputNode, SurfaceInputPlacement,
        SurfaceInputRect, SurfaceLayerPlacement, SurfacePlugin, SurfaceRenderImage,
        SurfaceTreeSnapshot, SurfaceWindowGeometry, enqueue_surface_event, take_surface_actions,
    };

    #[cfg(not(feature = "test-support"))]
    pub(crate) use crate::surface_impl::{
        HostSurfaceEvent, HostSurfaceEventKind, SurfaceBufferContent, SurfaceBufferUpdate,
        SurfaceContentView, SurfaceImageEncoding, SurfaceInputNode, SurfaceInputPlacement,
        SurfaceInputRect, SurfaceLayerPlacement, SurfacePlugin, SurfaceRenderImage,
        SurfaceTreeSnapshot, SurfaceWindowGeometry, enqueue_surface_event, take_surface_actions,
    };

    pub(crate) use crate::surface_impl::has_surface_frame;
}

/// Bevy version supported by Weld applications and plugins.
pub use bevy;
pub use builder::{ActiveBackend, Backend, WeldApp, WeldAppBuilder, WeldAppExt};

pub mod prelude {
    pub use crate::{
        ActiveBackend, Backend, WeldApp, WeldAppBuilder, WeldAppExt,
        output::OutputGeometry,
        surface::{
            AppPopup, AppWindow, ClientDecorated, ClientSurface, MappedSurface, ServerDecorated,
            SurfaceAction, SurfaceActionQueue, SurfaceId, SurfaceNode, SurfaceView,
            WindowDecoration, WindowInteractionRequest, WindowInteractionRequestKind,
            WindowResizeEdge,
        },
    };
}
