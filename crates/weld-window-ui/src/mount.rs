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
    bsn_list! {
        (
            template(move |_| Ok(SurfaceNode { surface, view }))
            Pickable::IGNORE
            LayoutConfig { use_rounding: true }
            Node { display: Display::None }
        )
    }
}
