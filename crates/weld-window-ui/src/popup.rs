//! Undecorated presentation for protocol-positioned xdg popups.

use bevy::{
    ecs::component::Component,
    picking::Pickable,
    prelude::{ChildOf, Children, Node, Overflow, PositionType, Query, Scene},
    scene::bsn,
};
use weld_app::surface::{MappedSurface, SurfaceAlphaMode, SurfaceId, SurfaceNode, SurfaceView};

use crate::{PopupProjection, surface_content_with_node};

#[derive(Component, Clone, Copy, Debug)]
pub(crate) struct PopupPresentation;

pub(super) fn content_view(mapped: MappedSurface) -> SurfaceView {
    match mapped.alpha_mode {
        SurfaceAlphaMode::Preserved => SurfaceView::FullSurface,
        SurfaceAlphaMode::Discarded => SurfaceView::WindowGeometry,
    }
}

fn content_overflow(view: SurfaceView) -> Overflow {
    match view {
        SurfaceView::FullSurface => Overflow::default(),
        SurfaceView::WindowGeometry => Overflow::clip(),
    }
}

/// Crop only the popup's mount, leaving it outside the owner's content clip.
/// Update in place so an alpha-mode transition preserves popup identity.
pub(super) fn sync_content(
    roots: Query<&PopupProjection>,
    surfaces: Query<&MappedSurface>,
    mut mounts: Query<(&ChildOf, &mut SurfaceNode, &mut Node)>,
) {
    for (parent, mut surface, mut node) in &mut mounts {
        let Ok(projection) = roots.get(parent.parent()) else {
            continue;
        };
        let Ok(mapped) = surfaces.get(projection.source) else {
            continue;
        };
        let view = content_view(*mapped);
        let overflow = content_overflow(view);
        if surface.view != view {
            surface.view = view;
        }
        if node.overflow != overflow {
            node.overflow = overflow;
        }
    }
}

pub(crate) fn scene(surface: SurfaceId, view: SurfaceView) -> impl Scene {
    let content = surface_content_with_node(
        surface,
        view,
        Node {
            overflow: content_overflow(view),
            ..Default::default()
        },
    );
    bsn! {
        Node { position_type: PositionType::Absolute }
        Pickable::IGNORE
        Children [{content}]
    }
}
