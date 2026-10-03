//! Animated X11 client for rootless mapping, input and hoist validation.

use anyhow::Result;
use clap::Parser;
use smithay::reexports::x11rb::{
    self,
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            AtomEnum, ChangeGCAux, ConnectionExt, CreateGCAux, CreateWindowAux, EventMask,
            PropMode, Rectangle, WindowClass,
        },
    },
    wrapper::ConnectionExt as _,
};
use std::{
    thread,
    time::{Duration, Instant},
};

#[derive(Parser)]
struct Options {
    #[arg(long, default_value_t = 60)]
    seconds: u64,
    #[arg(long, default_value = "Weld X11 probe")]
    title: String,
    #[arg(long)]
    lifecycle: bool,
}

fn main() -> Result<()> {
    let options = Options::parse();
    let (connection, screen) = x11rb::connect(None)?;
    let screen = &connection.setup().roots[screen];
    let window = connection.generate_id()?;
    connection
        .create_window(
            screen.root_depth,
            window,
            screen.root,
            40,
            40,
            640,
            480,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .background_pixel(0x204050)
                .event_mask(
                    EventMask::EXPOSURE
                        | EventMask::STRUCTURE_NOTIFY
                        | EventMask::KEY_PRESS
                        | EventMask::KEY_RELEASE
                        | EventMask::BUTTON_PRESS
                        | EventMask::BUTTON_RELEASE
                        | EventMask::FOCUS_CHANGE,
                ),
        )?
        .check()?;
    connection.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        options.title.as_bytes(),
    )?;
    connection.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"weld-x11-probe\0WeldX11Probe\0",
    )?;
    let protocols = connection
        .intern_atom(false, b"WM_PROTOCOLS")?
        .reply()?
        .atom;
    let delete = connection
        .intern_atom(false, b"WM_DELETE_WINDOW")?
        .reply()?
        .atom;
    connection.change_property32(
        PropMode::REPLACE,
        window,
        protocols,
        AtomEnum::ATOM,
        &[delete],
    )?;
    let gc = connection.generate_id()?;
    connection.create_gc(gc, window, &CreateGCAux::new().foreground(0x40c0a0))?;
    connection.map_window(window)?;
    connection.flush()?;
    let started = Instant::now();
    let mut width = 640;
    let mut height = 480;
    let mut frame = 0u16;
    let mut presses = 0u32;
    let mut phase = 0u8;
    let mut auxiliaries = Vec::new();
    println!(
        "X11_PROBE_READY window={window}; type or click to change the counter/color, close through Weld to test WM_DELETE_WINDOW"
    );
    while started.elapsed() < Duration::from_secs(options.seconds) {
        if options.lifecycle && phase == 0 && started.elapsed() >= Duration::from_secs(1) {
            for (title, popup, x, color) in [
                ("X11 dialog", false, 60, 0x405020),
                ("X11 popup", true, 100, 0x805030),
            ] {
                let child = connection.generate_id()?;
                connection
                    .create_window(
                        screen.root_depth,
                        child,
                        screen.root,
                        x,
                        100,
                        240,
                        120,
                        0,
                        WindowClass::INPUT_OUTPUT,
                        screen.root_visual,
                        &CreateWindowAux::new()
                            .background_pixel(color)
                            .override_redirect(u32::from(popup))
                            .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY),
                    )?
                    .check()?;
                connection.change_property8(
                    PropMode::REPLACE,
                    child,
                    AtomEnum::WM_NAME,
                    AtomEnum::STRING,
                    title.as_bytes(),
                )?;
                connection.change_property32(
                    PropMode::REPLACE,
                    child,
                    AtomEnum::WM_TRANSIENT_FOR,
                    AtomEnum::WINDOW,
                    &[window],
                )?;
                connection.map_window(child)?;
                auxiliaries.push(child);
            }
            phase = 1;
            println!("FAMILY_MAPPED");
        }
        if options.lifecycle && phase == 1 && started.elapsed() >= Duration::from_secs(5) {
            for child in auxiliaries.drain(..) {
                connection.destroy_window(child)?;
            }
            connection.unmap_window(window)?;
            phase = 2;
            println!("FAMILY_UNMAPPED");
        }
        if options.lifecycle && phase == 2 && started.elapsed() >= Duration::from_secs(6) {
            connection.map_window(window)?;
            connection.change_property8(
                PropMode::REPLACE,
                window,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                b"X11 probe remapped",
            )?;
            phase = 3;
            println!("ROOT_REMAPPED");
        }
        while let Some(event) = connection.poll_for_event()? {
            match event {
                Event::ConfigureNotify(event) if event.window == window => {
                    width = event.width;
                    height = event.height;
                    println!("CONFIGURE {width}x{height}");
                }
                Event::KeyPress(event) => {
                    presses += 1;
                    println!("KEY_PRESS {} count={presses}", event.detail);
                }
                Event::KeyRelease(event) => println!("KEY_RELEASE {}", event.detail),
                Event::ButtonPress(event) => {
                    presses += 1;
                    println!("BUTTON_PRESS {} count={presses}", event.detail);
                }
                Event::ButtonRelease(event) => println!("BUTTON_RELEASE {}", event.detail),
                Event::FocusIn(_) => println!("FOCUS_IN"),
                Event::FocusOut(_) => println!("FOCUS_OUT"),
                Event::ClientMessage(event)
                    if event.type_ == protocols && event.data.as_data32()[0] == delete =>
                {
                    println!("CLOSED_BY_WM");
                    return Ok(());
                }
                Event::Error(error) => anyhow::bail!("X11 error: {error:?}"),
                _ => {}
            }
        }
        connection.change_gc(
            gc,
            &ChangeGCAux::new().foreground(0x204050 ^ ((presses % 16) << 16)),
        )?;
        connection.poly_fill_rectangle(
            window,
            gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width,
                height,
            }],
        )?;
        connection.change_gc(gc, &ChangeGCAux::new().foreground(0x40c0a0))?;
        let x = i16::try_from(frame % width.max(1)).unwrap_or(i16::MAX);
        connection.poly_fill_rectangle(
            window,
            gc,
            &[Rectangle {
                x,
                y: 20,
                width: 30,
                height: height.saturating_sub(40),
            }],
        )?;
        connection.image_text8(
            window,
            gc,
            20,
            45,
            format!("X11: {} inputs", presses).as_bytes(),
        )?;
        connection.flush()?;
        frame = frame.wrapping_add(5);
        thread::sleep(Duration::from_millis(16));
    }
    connection.destroy_window(window)?;
    connection.flush()?;
    println!("X11_PROBE_DONE");
    Ok(())
}
