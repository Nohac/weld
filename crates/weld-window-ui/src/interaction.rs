//! Visual feedback for manager-owned window interactions.

use bevy::{
    ecs::{
        entity::Entity,
        message::MessageWriter,
        query::{Added, Without},
        system::{Commands, Query},
    },
    window::{CursorIcon, SystemCursorIcon},
};
use weld_app::{cursor::CursorRequest, surface::ToplevelResizeEdge};
use weld_window::{WindowInteractionKind, WindowInteractionSession, WindowResizeHandle};

type AddedResizeHandles<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static WindowResizeHandle),
    (Added<WindowResizeHandle>, Without<CursorIcon>),
>;

pub(crate) fn attach_resize_cursor_icons(mut commands: Commands, handles: AddedResizeHandles) {
    for (entity, handle) in &handles {
        commands
            .entity(entity)
            .insert(CursorIcon::System(resize_cursor_icon(handle.0)));
    }
}

const fn resize_cursor_icon(edge: ToplevelResizeEdge) -> SystemCursorIcon {
    match edge {
        ToplevelResizeEdge::Top | ToplevelResizeEdge::Bottom => SystemCursorIcon::NsResize,
        ToplevelResizeEdge::Left | ToplevelResizeEdge::Right => SystemCursorIcon::EwResize,
        ToplevelResizeEdge::TopLeft | ToplevelResizeEdge::BottomRight => {
            SystemCursorIcon::NwseResize
        }
        ToplevelResizeEdge::BottomLeft | ToplevelResizeEdge::TopRight => {
            SystemCursorIcon::NeswResize
        }
    }
}

pub(crate) fn request_interaction_cursor(
    interactions: Query<&WindowInteractionSession>,
    mut requests: MessageWriter<CursorRequest>,
) {
    let cursor = interactions
        .iter()
        .find_map(|interaction| match interaction.kind {
            WindowInteractionKind::Move => None,
            WindowInteractionKind::Resize(edge) => Some(resize_cursor_icon(edge)),
        });
    if let Some(cursor) = cursor {
        requests.write(CursorRequest(cursor));
    }
}
