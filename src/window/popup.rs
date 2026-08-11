//! Undecorated presentation for protocol-positioned xdg popups.

use bevy::{
    ecs::template::template,
    picking::Pickable,
    prelude::{Children, Display, Node, PositionType, Scene},
    scene::bsn,
    ui::LayoutConfig,
};

use crate::surface::{SurfaceId, SurfaceNode, SurfaceView};

#[derive(bevy::ecs::component::Component, Clone, Copy, Debug)]
pub(super) struct PopupPresentation;

pub(super) fn scene(surface: SurfaceId) -> impl Scene {
    bsn! {
        Node {
            position_type: PositionType::Absolute,
        }
        Pickable::IGNORE
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
