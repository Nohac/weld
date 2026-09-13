//! Typed cursor presentation on the Godot main thread, separate from video.
use godot::{
    classes::{
        Image, ImageTexture, Input,
        image::Format,
        input::{CursorShape, MouseMode},
    },
    prelude::*,
};
use std::sync::Once;
use weld_client::{ClientCursor, CursorIcon};

static FIRST_NAMED_CURSOR: Once = Once::new();

pub(super) fn reset() {
    let mut input = Input::singleton();
    input.set_mouse_mode(MouseMode::VISIBLE);
    input.set_custom_mouse_cursor(Gd::null_arg());
    input.set_default_cursor_shape();
}

pub(super) fn apply(cursor: ClientCursor) {
    // One diagnostic per process separates missing feedback from presentation
    // bugs without producing a log for every hover transition.
    if let ClientCursor::Named(icon) = &cursor
        && *icon != CursorIcon::Default
    {
        FIRST_NAMED_CURSOR.call_once(|| godot_print!("WELD_REMOTE_CURSOR first_named={icon:?}"));
    }
    let mut input = Input::singleton();
    input.set_mouse_mode(if matches!(cursor, ClientCursor::Hidden) {
        MouseMode::HIDDEN
    } else {
        MouseMode::VISIBLE
    });
    input.set_custom_mouse_cursor(Gd::null_arg());
    match cursor {
        ClientCursor::Hidden => {}
        ClientCursor::Named(icon) => input
            .set_default_cursor_shape_ex()
            .shape(shape(icon))
            .done(),
        ClientCursor::Image(image) => {
            input.set_default_cursor_shape();
            let texture = Image::create_from_data(
                i32::from(image.width()),
                i32::from(image.height()),
                false,
                Format::RGBA8,
                &PackedByteArray::from(image.rgba()),
            )
            .and_then(|image| ImageTexture::create_from_image(&image));
            if let Some(texture) = texture {
                input
                    .set_custom_mouse_cursor_ex(&texture)
                    .shape(CursorShape::ARROW)
                    .hotspot(Vector2::new(
                        f32::from(image.hotspot().0),
                        f32::from(image.hotspot().1),
                    ))
                    .done();
            } else {
                godot_warn!("Could not create remote cursor texture; using the default cursor");
            }
        }
    }
}

fn shape(icon: CursorIcon) -> CursorShape {
    match icon {
        CursorIcon::Pointer => CursorShape::POINTING_HAND,
        CursorIcon::Text | CursorIcon::VerticalText => CursorShape::IBEAM,
        CursorIcon::Crosshair => CursorShape::CROSS,
        CursorIcon::Wait => CursorShape::WAIT,
        CursorIcon::Progress => CursorShape::BUSY,
        CursorIcon::Help => CursorShape::HELP,
        CursorIcon::Move | CursorIcon::AllScroll => CursorShape::MOVE,
        CursorIcon::Grab | CursorIcon::Grabbing => CursorShape::DRAG,
        CursorIcon::NotAllowed | CursorIcon::NoDrop => CursorShape::FORBIDDEN,
        CursorIcon::EResize | CursorIcon::WResize | CursorIcon::EwResize => CursorShape::HSIZE,
        CursorIcon::NResize | CursorIcon::SResize | CursorIcon::NsResize => CursorShape::VSIZE,
        CursorIcon::NeResize | CursorIcon::SwResize | CursorIcon::NeswResize => {
            CursorShape::BDIAGSIZE
        }
        CursorIcon::NwResize | CursorIcon::SeResize | CursorIcon::NwseResize => {
            CursorShape::FDIAGSIZE
        }
        CursorIcon::ColResize => CursorShape::HSPLIT,
        CursorIcon::RowResize => CursorShape::VSPLIT,
        _ => CursorShape::ARROW,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cursor_shapes_map_without_engine_objects_or_string_protocols() {
        for (icon, expected) in [
            (CursorIcon::Default, CursorShape::ARROW),
            (CursorIcon::Text, CursorShape::IBEAM),
            (CursorIcon::Pointer, CursorShape::POINTING_HAND),
            (CursorIcon::NeswResize, CursorShape::BDIAGSIZE),
            (CursorIcon::NwseResize, CursorShape::FDIAGSIZE),
            (CursorIcon::ZoomIn, CursorShape::ARROW),
        ] {
            assert_eq!(shape(icon), expected);
        }
    }
}
