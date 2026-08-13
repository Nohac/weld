//! Bevy application integration and plugin-facing compositor model.

pub(crate) const PROFILE_TARGET: &str = "weld_profile";

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
        ClientDecorated, ClientPopup, ClientSurface, ClientToplevel, MappedSurface,
        ServerDecorated, SurfaceAction, SurfaceActionQueue, SurfaceCommitRevisions, SurfaceId,
        SurfaceLayerId, SurfaceNode, SurfaceSystems, SurfaceView, ToplevelInteractionRequest,
        ToplevelInteractionRequestKind, ToplevelResizeEdge, WindowDecoration,
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

    pub(crate) use crate::surface_impl::{
        has_surface_frame, promote_dmabuf_sources, publish_surface_bindings, referenced_dmabuf_ids,
        reject_dmabuf_sources,
    };
}

/// Bevy version supported by Weld applications and plugins.
pub use bevy;
pub use builder::{ActiveBackend, Backend, WeldApp, WeldAppBuilder, WeldAppExt};
pub use weld_core::OutputScale;

pub mod prelude {
    pub use crate::{
        ActiveBackend, Backend, OutputScale, WeldApp, WeldAppBuilder, WeldAppExt,
        output::OutputGeometry,
        surface::{
            ClientDecorated, ClientPopup, ClientSurface, ClientToplevel, MappedSurface,
            ServerDecorated, SurfaceAction, SurfaceActionQueue, SurfaceId, SurfaceNode,
            SurfaceView, ToplevelInteractionRequest, ToplevelInteractionRequestKind,
            ToplevelResizeEdge, WindowDecoration,
        },
    };
}
