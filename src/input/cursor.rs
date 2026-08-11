//! Bevy-composed software cursor presentation.

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        component::Component,
        message::MessageWriter,
        query::With,
        system::{Query, Res},
    },
    prelude::{BackgroundColor, BorderRadius, Color, GlobalZIndex, Scene, px},
    ui::{Display, Node, PositionType},
    window::RequestRedraw,
};

use super::state::ProjectedPointerState;
use crate::layer::CURSOR_Z_INDEX;

#[derive(Clone, Component, Default)]
pub(crate) struct SoftwareCursor;

pub(crate) struct SoftwareCursorPlugin;

impl Plugin for SoftwareCursorPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PreUpdate, update_software_cursor);
    }
}

pub(crate) fn software_cursor_scene() -> impl Scene {
    use bevy::picking::Pickable;
    use bevy::scene::bsn;

    bsn! {
        Node {
            display: Display::None,
            position_type: PositionType::Absolute,
            width: px(14),
            height: px(14),
            border_radius: BorderRadius::all(px(7)),
        }
        BackgroundColor(Color::WHITE)
        GlobalZIndex(CURSOR_Z_INDEX)
        Pickable::IGNORE
        SoftwareCursor
    }
}

fn update_software_cursor(
    pointer: Res<ProjectedPointerState>,
    mut cursors: Query<&mut Node, With<SoftwareCursor>>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    for mut node in &mut cursors {
        let next = pointer.0.host_position;
        let next_display = if next.is_some() {
            Display::Flex
        } else {
            Display::None
        };
        let (left, top) = next
            .map(|position| (px(position.x as f32 - 7.0), px(position.y as f32 - 7.0)))
            .unwrap_or((px(0), px(0)));
        if node.display != next_display || node.left != left || node.top != top {
            node.display = next_display;
            node.left = left;
            node.top = top;
            redraw.write(RequestRedraw);
        }
    }
}
