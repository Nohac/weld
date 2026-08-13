//! Baseline client-decorated window presentation.

use bevy::{
    color::Color,
    ecs::{component::Component, template::template},
    picking::Pickable,
    prelude::{BoxShadow, Children, Node, PositionType, Scene, px},
    scene::bsn,
};
use weld_app::surface::{SurfaceId, SurfaceView};

use crate::surface_content;

#[derive(Component, Clone, Copy, Debug)]
pub(crate) struct ClientWindowPresentation;

pub(crate) fn scene(surface: SurfaceId) -> impl Scene {
    let content = surface_content(surface, SurfaceView::FullSurface);
    bsn! {
        Node { position_type: PositionType::Absolute }
        Pickable::IGNORE
        template(|_| Ok(fallback_shadow()))
        Children [{content}]
    }
}

pub(crate) fn fallback_shadow() -> BoxShadow {
    BoxShadow::new(
        Color::srgba(0.0, 0.0, 0.0, 0.55),
        px(0),
        px(12),
        px(2),
        px(24),
    )
}
