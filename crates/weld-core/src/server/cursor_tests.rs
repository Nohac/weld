//! Cursor feedback must unblock subsequent client cursor frames without video.

use super::*;
use smithay::{
    backend::input::InputTime,
    input::pointer::{CursorImageStatus, MotionEvent},
    reexports::wayland_server::protocol::wl_surface::WlSurface as ServerSurface,
    utils::SERIAL_COUNTER,
    wayland::{compositor::give_role, seat::CURSOR_IMAGE_ROLE},
};

#[test]
#[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
fn cursor_frames_complete_across_focus_changes_without_window_presentation() {
    let mut f = Fixture::new();
    let root = f.surface(301);
    let (_, _xdg) = f.toplevel(&root);
    root.attach(Some(&f.buffer()), 0, 0);
    root.commit();
    let cursor = f.surface(302);
    let other_cursor = f.surface(303);
    f.sync();
    let native_root = f
        .client
        .object_from_protocol_id::<ServerSurface>(&f.server.display_handle, root.id().protocol_id())
        .expect("root");
    let native_cursor = f
        .client
        .object_from_protocol_id::<ServerSurface>(
            &f.server.display_handle,
            cursor.id().protocol_id(),
        )
        .expect("cursor");
    give_role(&native_cursor, CURSOR_IMAGE_ROLE).expect("cursor role");
    let other_native_cursor = f
        .client
        .object_from_protocol_id::<ServerSurface>(
            &f.server.display_handle,
            other_cursor.id().protocol_id(),
        )
        .expect("other cursor");
    give_role(&other_native_cursor, CURSOR_IMAGE_ROLE).expect("other cursor role");
    let pointer = f.server.seat.get_pointer().expect("pointer");
    pointer.motion(
        &mut f.server,
        Some((native_root.clone(), (0.0, 0.0).into())),
        &MotionEvent {
            location: (1.0, 1.0).into(),
            serial: SERIAL_COUNTER.next_serial(),
            time: InputTime::from_millis(1),
        },
    );
    f.server.set_shell_cursor_ownership(false);
    f.server
        .set_client_cursor_image(CursorImageStatus::Surface(native_cursor));
    for expected in 1..=4 {
        if expected == 2 || expected == 3 {
            let entering = expected == 3;
            pointer.motion(
                &mut f.server,
                entering.then(|| (native_root.clone(), (0.0, 0.0).into())),
                &MotionEvent {
                    location: (1.0, 1.0).into(),
                    serial: SERIAL_COUNTER.next_serial(),
                    time: InputTime::from_millis(expected),
                },
            );
            f.server.set_shell_cursor_ownership(!entering);
        } else if expected == 4 {
            f.server
                .set_client_cursor_image(CursorImageStatus::Surface(other_native_cursor.clone()));
        }
        cursor.attach(Some(&f.buffer()), 0, 0);
        cursor.frame(&f.queue.handle(), true);
        cursor.commit();
        f.sync();
        assert_eq!(
            f.observer.frames, expected as usize,
            "consumed cursor frame completes regardless of focus"
        );
        f.server.flush_cursor_feedback();
        f.sync();
        assert_eq!(
            f.observer.frames, expected as usize,
            "callback completes only once"
        );
    }
}
