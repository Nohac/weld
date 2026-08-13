//! Undecorated presentation for protocol-positioned xdg popups.

use bevy::{
    ecs::component::Component,
    picking::Pickable,
    prelude::{Children, Node, PositionType, Scene},
    scene::bsn,
};
use weld_app::surface::{SurfaceId, SurfaceView};

use crate::surface_content;

#[derive(Component, Clone, Copy, Debug)]
pub(crate) struct PopupPresentation;

pub(crate) fn scene(surface: SurfaceId) -> impl Scene {
    let content = surface_content(surface, SurfaceView::FullSurface);
    bsn! {
        Node { position_type: PositionType::Absolute }
        Pickable::IGNORE
        Children [{content}]
    }
}
