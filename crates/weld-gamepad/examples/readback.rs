//! Explicit bounded host probe; never run as part of unit tests or as root.
use evdev::{AbsoluteAxisCode, Device, EventType, KeyCode};
use std::{
    io, thread,
    time::{Duration, Instant},
};
use weld_gamepad::UinputGamepad;
use weld_hoist_core::gamepad::GamepadState;

fn main() -> io::Result<()> {
    let mut pad = UinputGamepad::create()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut reader = loop {
        if let Some(path) = pad.event_nodes()?.into_iter().next() {
            match Device::open(&path) {
                Ok(device) => {
                    println!("Unprivileged readback: {}", path.display());
                    break device;
                }
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => {}
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(
                "gamepad event node unavailable; check input-device read permissions",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    };
    reader.set_nonblocking(true)?;
    let mut state = GamepadState {
        left: [12345, -23456],
        ..Default::default()
    };
    state.buttons.south = true;
    pad.write_state(state)?;
    pad.write_state(GamepadState::default())?;
    let mut press = false;
    let mut release = false;
    let mut axis = false;
    let mut neutral = false;
    while Instant::now() < deadline {
        match reader.fetch_events() {
            Ok(events) => {
                for event in events {
                    match (event.event_type(), event.code(), event.value()) {
                        (EventType::KEY, code, 1) if code == KeyCode::BTN_SOUTH.0 => press = true,
                        (EventType::KEY, code, 0) if code == KeyCode::BTN_SOUTH.0 => release = true,
                        (EventType::ABSOLUTE, code, 12345) if code == AbsoluteAxisCode::ABS_X.0 => {
                            axis = true
                        }
                        (EventType::ABSOLUTE, code, 0) if code == AbsoluteAxisCode::ABS_X.0 => {
                            neutral = true
                        }
                        _ => {}
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if press && release && axis && neutral {
            // Optional bounded observation window for SDL's separate discovery
            // process; normal readback exits immediately.
            if std::env::args().any(|argument| argument == "--inspect-sdl") {
                println!("Readback passed; holding neutral device for SDL inspection (5s)");
                thread::sleep(Duration::from_secs(5));
            }
            drop(pad);
            println!(
                "PASS: press, release, analog and neutral read through separate event node; device dropped"
            );
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(io::Error::other("gamepad readback timed out"))
}
