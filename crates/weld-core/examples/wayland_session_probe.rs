//! Subprocess regression fixture for `scripts/check-host-runtime`.
//! Real protocol traffic, independent of core's internal test helpers.

use std::{
    fs::File,
    io::Write,
    os::fd::AsFd,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use rustix::fs::{MemfdFlags, memfd_create};
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool,
        wl_surface,
    },
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct Probe {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<xdg_wm_base::XdgWmBase>,
    configured: usize,
    size: (i32, i32),
    closed: bool,
    busy: [bool; 2],
    frames: usize,
    releases: usize,
    output: (i32, i32, i32),
    scale: i32,
    frame_by_slot: [Option<usize>; 2],
    timings: Vec<FrameTiming>,
}

struct FrameTiming {
    committed: Instant,
    callback: Option<Duration>,
    released: Option<Duration>,
}

#[derive(Parser)]
struct Expectations {
    /// A host without a presenter must accept commits/releases but not pace frames.
    #[arg(long)]
    dormant: bool,
    /// Print bounded per-frame timings on the producer's own clock.
    #[arg(long)]
    frame_timings: bool,
    #[arg(long, value_parser = parse_extent)]
    expected_output: (i32, i32),
    #[arg(long)]
    expected_scale: i32,
    #[arg(long, value_parser = parse_extent)]
    expected_size: (i32, i32),
}

fn parse_extent(value: &str) -> Result<(i32, i32), String> {
    let (width, height) = value.split_once('x').ok_or("expected WIDTHxHEIGHT")?;
    let width = width.parse::<i32>().map_err(|error| error.to_string())?;
    let height = height.parse::<i32>().map_err(|error| error.to_string())?;
    if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
        return Err("expected dimensions within 1..8192".to_owned());
    }
    Ok((width, height))
}

fn main() -> Result<()> {
    let expected = Expectations::parse();
    let connection = Connection::connect_to_env()?;
    let mut queue = connection.new_event_queue();
    let handle = queue.handle();
    let mut probe = Probe::default();
    connection.display().get_registry(&handle, ());
    queue.roundtrip(&mut probe)?;
    queue.roundtrip(&mut probe)?;
    ensure!(
        probe.output
            == (
                expected.expected_output.0,
                expected.expected_output.1,
                60_000
            ),
        "unexpected output {:?}",
        probe.output
    );
    ensure!(
        probe.scale == expected.expected_scale,
        "fractional scale must advertise integer ceiling"
    );
    let surface = probe
        .compositor
        .as_ref()
        .context("missing compositor")?
        .create_surface(&handle, ());
    let xdg = probe
        .shell
        .as_ref()
        .context("missing xdg shell")?
        .get_xdg_surface(&surface, &handle, ());
    let top = xdg.get_toplevel(&handle, ());
    top.set_title("Weld runtime regression fixture".to_owned());
    top.set_min_size(1100, 200);
    top.set_max_size(1600, 300);
    surface.commit();
    while probe.configured == 0 {
        queue.blocking_dispatch(&mut probe)?;
        ensure!(!probe.closed, "unexpected close before configure");
    }
    ensure!(
        probe.size == expected.expected_size,
        "initial size ignored client constraints: {:?}",
        probe.size
    );
    // The window deliberately exceeds the advertised output. Output geometry
    // must not act as a workspace bound or clamp application geometry.
    let (width, height) = probe.size;
    println!("configured {width}x{height}");
    let bytes = width * height * 4;
    let mut storage = File::from(memfd_create("weld-runtime-probe", MemfdFlags::CLOEXEC)?);
    storage.write_all(&vec![0x60; usize::try_from(bytes * 2)?])?;
    let pool = probe.shm.as_ref().context("missing SHM")?.create_pool(
        storage.as_fd(),
        bytes * 2,
        &handle,
        (),
    );
    let buffers = [0, 1].map(|slot| {
        pool.create_buffer(
            bytes * slot,
            width,
            height,
            width * 4,
            wl_shm::Format::Xrgb8888,
            &handle,
            slot as usize,
        )
    });
    pool.destroy();
    drop(storage);
    let started = Instant::now();
    if expected.dormant {
        surface.attach(Some(&buffers[0]), 0, 0);
        surface.frame(&handle, None);
        surface.commit();
        while started.elapsed() < Duration::from_millis(300) {
            queue.roundtrip(&mut probe)?;
            std::thread::sleep(Duration::from_millis(10));
        }
        ensure!(
            probe.frames == 0,
            "unviewed surface received a frame callback"
        );
        ensure!(
            probe.releases > 0,
            "buffer release incorrectly depends on frame callbacks"
        );
        println!("PASS dormant: commits serviced, buffers released, zero frame callbacks");
        return Ok(());
    }
    for frame in 0..60 {
        let slot = probe
            .busy
            .iter()
            .position(|busy| !busy)
            .context("both SHM buffers retained")?;
        probe.busy[slot] = true;
        probe.frame_by_slot[slot] = Some(frame);
        probe.timings.push(FrameTiming {
            committed: Instant::now(),
            callback: None,
            released: None,
        });
        surface.attach(Some(&buffers[slot]), 0, 0);
        surface.damage(0, 0, width, height);
        surface.frame(&handle, Some(frame));
        surface.commit();
        while probe.frames == frame {
            queue.blocking_dispatch(&mut probe)?;
            ensure!(!probe.closed, "unexpected close during frame test");
        }
    }
    queue.roundtrip(&mut probe)?;
    if expected.frame_timings {
        for (frame, timing) in probe.timings.iter().enumerate() {
            println!(
                "frame={} callback_us={:?} buffer_release_us={:?}",
                frame + 1,
                timing.callback.map(|duration| duration.as_micros()),
                timing.released.map(|duration| duration.as_micros())
            );
        }
    }
    let elapsed = started.elapsed().as_millis();
    ensure!(probe.frames == 60, "callbacks duplicated during handoff");
    println!(
        "frames={} releases={} elapsed_ms={elapsed}",
        probe.frames, probe.releases
    );
    ensure!(
        (800..3000).contains(&elapsed),
        "callback cadence: {elapsed} ms"
    );
    ensure!(
        probe.releases >= 58,
        "only {} buffer releases",
        probe.releases
    );

    let previous_configures = probe.configured;
    surface.attach(None, 0, 0);
    surface.commit();
    queue.roundtrip(&mut probe)?;
    top.set_min_size(0, 0);
    top.set_max_size(0, 0);
    surface.commit();
    while probe.configured == previous_configures {
        queue.blocking_dispatch(&mut probe)?;
        ensure!(!probe.closed, "unexpected close during remap");
    }
    ensure!(
        probe.size == (0, 0),
        "startup default reapplied on remap: {:?}",
        probe.size
    );
    surface.attach(Some(&buffers[0]), 0, 0);
    surface.damage(0, 0, width, height);
    surface.frame(&handle, None);
    surface.commit();
    while probe.frames == 60 {
        queue.blocking_dispatch(&mut probe)?;
        ensure!(!probe.closed, "unexpected close after remap");
    }
    ensure!(probe.frames == 61, "callbacks duplicated during remap");
    println!(
        "PASS configure={width}x{height} output={:?} scale={} frames={} releases={} elapsed_ms={elapsed} remap=0x0",
        probe.output, probe.scale, probe.frames, probe.releases
    );
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), handle, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, handle, ())),
                "xdg_wm_base" => state.shell = Some(registry.bind(name, 1, handle, ())),
                "wl_output" => {
                    registry.bind::<wl_output::WlOutput, _, _>(name, version.min(2), handle, ());
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Probe {
    fn event(
        _: &mut Self,
        shell: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            shell.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Probe {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.configured += 1;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => state.size = (width, height),
            xdg_toplevel::Event::Close => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, Option<usize>> for Probe {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        frame: &Option<usize>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.frames += 1;
        if let Some(frame) = frame {
            let timing = &mut state.timings[*frame];
            timing.callback = Some(timing.committed.elapsed());
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for Probe {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        slot: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.busy[*slot] = false;
        state.releases += 1;
        if let Some(frame) = state.frame_by_slot[*slot].take() {
            let timing = &mut state.timings[frame];
            timing.released = Some(timing.committed.elapsed());
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                refresh,
            } if flags.contains(wl_output::Mode::Current) => {
                state.output = (width, height, refresh)
            }
            wl_output::Event::Scale { factor } => state.scale = factor,
            _ => {}
        }
    }
}

delegate_noop!(Probe: ignore wl_compositor::WlCompositor);
delegate_noop!(Probe: ignore wl_shm::WlShm);
delegate_noop!(Probe: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Probe: ignore wl_surface::WlSurface);
