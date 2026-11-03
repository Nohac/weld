//! XWayland pointer grabs and cursor hiding through the actual X11 server.

use super::*;
use anyhow::{Context, Result, ensure};
use smithay::reexports::x11rb::{
    self,
    connection::Connection as _,
    protocol::{
        Event,
        xfixes::ConnectionExt as _,
        xproto::{
            ConnectionExt as _, CreateWindowAux, EventMask, GrabMode, GrabStatus, WindowClass,
        },
    },
};
use std::{process::Command, sync::mpsc, thread};

fn run_client(
    display: &str,
    commands: mpsc::Receiver<()>,
    replies: &mpsc::Sender<Result<&'static str, String>>,
) -> Result<()> {
    let (connection, screen) = x11rb::connect(Some(display))?;
    let screen = &connection.setup().roots[screen];
    connection.xfixes_query_version(5, 0)?.reply()?;
    let window = connection.generate_id()?;
    connection
        .create_window(
            screen.root_depth,
            window,
            screen.root,
            0,
            0,
            64,
            64,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .background_pixel(0x30_60_90)
                .event_mask(EventMask::POINTER_MOTION | EventMask::STRUCTURE_NOTIFY),
        )?
        .check()?;
    connection.map_window(window)?;
    connection.flush()?;
    replies.send(Ok("mapped"))?;
    commands
        .recv_timeout(Duration::from_secs(10))
        .context("grab command")?;
    let grab = connection
        .grab_pointer(
            false,
            window,
            EventMask::POINTER_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            window,
            x11rb::NONE,
            x11rb::CURRENT_TIME,
        )?
        .reply()?;
    ensure!(
        grab.status == GrabStatus::SUCCESS,
        "XGrabPointer failed: {:?}",
        grab.status
    );
    connection.xfixes_hide_cursor(window)?;
    connection.flush()?;
    while connection.poll_for_event()?.is_some() {}
    replies.send(Ok("grabbed"))?;
    commands
        .recv_timeout(Duration::from_secs(10))
        .context("motion command")?;
    // A reply establishes that the X server processed preceding input.
    connection.get_input_focus()?.reply()?;
    let mut motion = false;
    while let Some(event) = connection.poll_for_event()? {
        motion |= matches!(event, Event::MotionNotify(_));
    }
    ensure!(motion, "X11 grab did not receive pointer movement");
    connection.ungrab_pointer(x11rb::CURRENT_TIME)?;
    connection.xfixes_show_cursor(window)?;
    connection.flush()?;
    replies.send(Ok("released"))?;
    commands
        .recv_timeout(Duration::from_secs(10))
        .context("finish command")?;
    connection.destroy_window(window)?;
    connection.flush()?;
    Ok(())
}

fn pump_until(f: &mut Fixture, mut ready: impl FnMut(&Fixture) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready(f) {
        assert!(Instant::now() < deadline, "XWayland condition timed out");
        f.sync();
        let frame = f.server.stage_frame_callbacks();
        f.server.complete_frame_callbacks(frame);
    }
}
fn reply(f: &mut Fixture, replies: &mpsc::Receiver<Result<&'static str, String>>, expected: &str) {
    let mut result = None;
    pump_until(f, |_| match replies.try_recv() {
        Ok(value) => {
            result = Some(value);
            true
        }
        Err(mpsc::TryRecvError::Empty) => false,
        Err(error) => panic!("X11 client stopped: {error}"),
    });
    assert_eq!(result.expect("reply").expect("X11 client"), expected);
}

#[test]
#[ignore = "starts a private XWayland server; requires Xwayland and XDG_RUNTIME_DIR"]
fn xwayland_pointer_grab_hides_cursor_delivers_motion_and_releases() {
    // XWM callbacks retain event-loop handles. Process isolation gives this
    // external-server fixture a complete teardown before other socket tests.
    const CHILD: &str = "WELD_POINTER_X11_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let thread = thread::current();
        let test = thread.name().expect("named test thread");
        let status = Command::new(std::env::current_exe().expect("test executable"))
            .args([test, "--exact", "--ignored", "--nocapture"])
            .env(CHILD, "1")
            .status()
            .expect("isolated XWayland test");
        assert!(status.success(), "XWayland subprocess failed: {status}");
        return;
    }
    let mut f = Fixture::new();
    f.server
        .start_xwayland(f.event_loop.handle())
        .expect("start XWayland");
    pump_until(&mut f, |f| f.server.xwayland.wm.is_some());
    let display = format!(":{}", f.server.x11_display().expect("X11 display"));
    let (commands, receive_commands) = mpsc::channel();
    let (send_replies, replies) = mpsc::channel();
    let client = thread::spawn(move || {
        if let Err(error) = run_client(&display, receive_commands, &send_replies) {
            let _ = send_replies.send(Err(format!("{error:#}")));
        }
    });
    reply(&mut f, &replies, "mapped");
    pump_until(&mut f, |f| {
        f.server
            .toplevels
            .values()
            .any(|window| window.tree.client_mapped(window.surface.wl_surface()))
    });
    let root = f
        .server
        .toplevels
        .values()
        .find(|window| window.tree.client_mapped(window.surface.wl_surface()))
        .expect("X11 window")
        .surface
        .wl_surface()
        .clone();
    let window = f
        .server
        .toplevels
        .id_for_surface(&root)
        .expect("window identity");
    let route = route(window);
    f.server.route_controller_focus(None, Some(window));
    motion(&mut f, route, InputPosition::new(10., 10.), None);
    commands.send(()).expect("grab");
    reply(&mut f, &replies, "grabbed");
    pump_until(&mut f, |f| f.server.local_pointer_constraint().is_some());
    let input = f
        .server
        .local_input
        .pointer_binding
        .borrow()
        .clone()
        .expect("X11 input");
    pump_until(&mut f, |_| {
        *input.cursor_status.borrow() == CursorImageStatus::Hidden
    });
    motion(
        &mut f,
        route,
        InputPosition::new(15., 12.),
        Some(RelativeMotion {
            delta: InputDelta::new(5., 2.),
            unaccelerated: InputDelta::new(5., 2.),
            time_micros: 20_000,
        }),
    );
    commands.send(()).expect("read motion");
    reply(&mut f, &replies, "released");
    pump_until(&mut f, |f| f.server.local_pointer_constraint().is_none());
    commands.send(()).expect("finish");
    client.join().expect("X11 worker");
}
