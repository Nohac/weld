//! Shared client-surface mounts used by window presenters.

use bevy::{
    ecs::template::template,
    picking::Pickable,
    prelude::{Display, Node},
    scene::{SceneList, bsn_list},
    ui::LayoutConfig,
};
use weld_app::surface::{SurfaceId, SurfaceNode, SurfaceView};

/// Creates the consistently configured client-content node for a presentation.
pub fn surface_content(surface: SurfaceId, view: SurfaceView) -> impl SceneList {
    surface_content_with_node(surface, view, Node::default())
}

/// Creates a client-content node with presentation-specific layout behavior.
pub fn surface_content_with_node(
    surface: SurfaceId,
    view: SurfaceView,
    mut node: Node,
) -> impl SceneList {
    node.display = Display::None;
    bsn_list! {
        (
            template(move |_| Ok(SurfaceNode { surface, view }))
            Pickable::IGNORE
            LayoutConfig { use_rounding: true }
            template(move |_| Ok(node.clone()))
        )
    }
}
