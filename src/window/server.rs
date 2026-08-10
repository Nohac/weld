//! Server-side-decorated application-window presentation.

use bevy::{
    color::Color,
    ecs::template::template,
    picking::Pickable,
    prelude::{
        AlignItems, BackgroundColor, BorderColor, BorderRadius, Button, Children, Display,
        FlexDirection, ImageNode, JustifyContent, Node, Overflow, PositionType, Rot2, Scene,
        UiRect, UiTransform, percent, px,
    },
    scene::{bsn, on},
    ui::LayoutConfig,
};

use crate::surface::{SurfaceId, SurfaceNode, SurfaceView};

use super::{
    BORDER_WIDTH, CLOSE_BUTTON_SIZE, HEADER_HEIGHT, INNER_BORDER_RADIUS, OUTER_BORDER_RADIUS,
    UNFOCUSED_BORDER, WindowBody, WindowHeader, close_window, window_shadow,
};

#[derive(bevy::ecs::component::Component, Clone, Copy, Debug)]
pub(super) struct ServerWindowPresentation;

pub(super) fn scene(surface: SurfaceId) -> impl Scene {
    bsn! {
        Node {
            position_type: PositionType::Absolute,
            flex_direction: FlexDirection::Column,
            border: UiRect::all(px(BORDER_WIDTH)),
            border_radius: BorderRadius::all(px(OUTER_BORDER_RADIUS)),
        }
        BorderColor::all(UNFOCUSED_BORDER)
        template(|_| Ok(window_shadow()))
        Children [(
            WindowBody
            Node {
                flex_direction: FlexDirection::Column,
                border_radius: BorderRadius::all(px(INNER_BORDER_RADIUS)),
                overflow: Overflow::clip(),
            }
            BackgroundColor(Color::srgb(0.10, 0.12, 0.16))
            Children [
                (
                    WindowHeader
                    Node {
                        width: percent(100),
                        height: px(HEADER_HEIGHT),
                        flex_shrink: 0.0,
                        align_items: AlignItems::Center,
                        justify_content: JustifyContent::FlexEnd,
                        border_radius: BorderRadius::px(
                            INNER_BORDER_RADIUS,
                            INNER_BORDER_RADIUS,
                            0.0,
                            0.0,
                        ),
                    }
                    BackgroundColor(Color::srgb(0.14, 0.17, 0.22))
                    Children [(
                        Button
                        Node {
                            width: px(CLOSE_BUTTON_SIZE),
                            height: px(CLOSE_BUTTON_SIZE),
                            margin: UiRect::right(px(4)),
                            align_items: AlignItems::Center,
                            justify_content: JustifyContent::Center,
                            border_radius: BorderRadius::MAX,
                        }
                        BackgroundColor(Color::srgb(0.54, 0.16, 0.18))
                        on(close_window(surface))
                        Children [(
                            Pickable::IGNORE
                            Node {
                                width: px(12),
                                height: px(12),
                                position_type: PositionType::Relative,
                            }
                            Children [
                                (
                                    Pickable::IGNORE
                                    Node {
                                        position_type: PositionType::Absolute,
                                        left: px(0),
                                        top: px(5),
                                        width: px(12),
                                        height: px(2),
                                    }
                                    UiTransform::from_rotation(Rot2::degrees(45.0))
                                    BackgroundColor(Color::WHITE)
                                ),
                                (
                                    Pickable::IGNORE
                                    Node {
                                        position_type: PositionType::Absolute,
                                        left: px(0),
                                        top: px(5),
                                        width: px(12),
                                        height: px(2),
                                    }
                                    UiTransform::from_rotation(Rot2::degrees(-45.0))
                                    BackgroundColor(Color::WHITE)
                                ),
                            ]
                        )]
                    )]
                ),
                (
                    template(move |_| Ok(SurfaceNode {
                        surface,
                        view: SurfaceView::WindowGeometry,
                    }))
                    Pickable::IGNORE
                    LayoutConfig { use_rounding: true }
                    ImageNode::default()
                    Node {
                        display: Display::None,
                        overflow: Overflow::clip(),
                        border_radius: BorderRadius::px(
                            0.0,
                            0.0,
                            INNER_BORDER_RADIUS,
                            INNER_BORDER_RADIUS,
                        ),
                    }
                ),
            ]
        )]
    }
}
