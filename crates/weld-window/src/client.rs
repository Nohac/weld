//! Client-side-decorated application-window presentation.

use bevy::{
    ecs::{component::Component, template::template},
    picking::Pickable,
    prelude::{Children, Display, Node, PositionType, Scene},
    scene::bsn,
    ui::LayoutConfig,
};

use weld_app::surface::{SurfaceId, SurfaceNode, SurfaceView};

use super::window_shadow;

#[derive(Component, Clone, Copy, Debug)]
pub(super) struct ClientWindowPresentation;

pub(super) fn scene(surface: SurfaceId) -> impl Scene {
    bsn! {
        Node {
            position_type: PositionType::Absolute,
        }
        Pickable::IGNORE
        template(|_| Ok(window_shadow()))
        Children [(
            template(move |_| Ok(SurfaceNode {
                surface,
                view: SurfaceView::FullSurface,
            }))
            Pickable::IGNORE
            LayoutConfig { use_rounding: true }
            Node {
                display: Display::None,
            }
        )]
    }
}
