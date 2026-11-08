//! Capture through real Wayland DMA-BUF and SHM buffers; compare static pixels
//! and exercise buffer reuse at a bounded requested cadence. Run with a timeout.
use anyhow::{Context, Result, ensure};
use clap::Parser;
use smithay::backend::{
    allocator::{
        Allocator, Buffer, Fourcc, Modifier,
        dmabuf::{AsDmabuf, DmabufMappingMode, DmabufSyncFlags},
        gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
    },
    drm::DrmNode,
};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::fd::AsFd,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
};
use wayland_protocols::{
    ext::{
        foreign_toplevel_list::v1::client::{
            ext_foreign_toplevel_handle_v1 as window_handle,
            ext_foreign_toplevel_list_v1 as window_list,
        },
        image_capture_source::v1::client::{
            ext_foreign_toplevel_image_capture_source_manager_v1 as window_sources,
            ext_image_capture_source_v1 as source,
            ext_output_image_capture_source_manager_v1 as sources,
        },
        image_copy_capture::v1::client::{
            ext_image_copy_capture_frame_v1 as frames, ext_image_copy_capture_manager_v1 as copies,
            ext_image_copy_capture_session_v1 as sessions,
        },
    },
    wp::linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_v1 as dma,
    },
};

#[derive(Parser)]
struct Options {
    /// Expected RGB at all four corners of the SHM capture (six hex digits).
    #[arg(long, requires = "compare")]
    corner_rgb: Option<String>,
    /// Marker prefix for a driver that resizes and then closes this window.
    #[arg(long)]
    lifecycle: Option<std::path::PathBuf>,
    /// Capture a window by its exact foreign-toplevel title.
    #[arg(long)]
    window_title: Option<String>,
    #[arg(long, default_value_t = 120)]
    frames: u32,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    #[arg(long)]
    cursor: bool,
    /// Require a static source and compare its SHM and DMA-BUF pixels exactly.
    #[arg(long)]
    compare: bool,
}
#[derive(Default)]
struct Probe {
    windows: Vec<(window_handle::ExtForeignToplevelHandleV1, String)>,
    window_sources: Option<window_sources::ExtForeignToplevelImageCaptureSourceManagerV1>,
    output: Option<wl_output::WlOutput>,
    sources: Option<sources::ExtOutputImageCaptureSourceManagerV1>,
    copies: Option<copies::ExtImageCopyCaptureManagerV1>,
    dma: Option<dma::ZwpLinuxDmabufV1>,
    shm: Option<wl_shm::WlShm>,
    size: (u32, u32),
    node: Option<DrmNode>,
    formats: Vec<(u32, u64)>,
    ready: bool,
    done: bool,
    failure: Option<String>,
}
delegate_noop!(Probe: ignore wl_output::WlOutput);
delegate_noop!(Probe: ignore window_sources::ExtForeignToplevelImageCaptureSourceManagerV1);
delegate_noop!(Probe: ignore wl_shm::WlShm);
delegate_noop!(Probe: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Probe: ignore wl_buffer::WlBuffer);
delegate_noop!(Probe: ignore sources::ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(Probe: ignore source::ExtImageCaptureSourceV1);
delegate_noop!(Probe: ignore copies::ExtImageCopyCaptureManagerV1);
delegate_noop!(Probe: ignore dma::ZwpLinuxDmabufV1);
delegate_noop!(Probe: ignore params::ZwpLinuxBufferParamsV1);
impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_output" if state.output.is_none() => {
                    state.output = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "ext_foreign_toplevel_list_v1" => {
                    let _: window_list::ExtForeignToplevelListV1 = registry.bind(name, 1, qh, ());
                }
                "ext_foreign_toplevel_image_capture_source_manager_v1" => {
                    state.window_sources = Some(registry.bind(name, 1, qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "zwp_linux_dmabuf_v1" => {
                    state.dma = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.sources = Some(registry.bind(name, 1, qh, ()))
                }
                "ext_image_copy_capture_manager_v1" => {
                    state.copies = Some(registry.bind(name, 1, qh, ()))
                }
                _ => {}
            }
        }
    }
}
impl Dispatch<window_list::ExtForeignToplevelListV1, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &window_list::ExtForeignToplevelListV1,
        event: window_list::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let window_list::Event::Toplevel { toplevel } = event {
            state.windows.push((toplevel, String::new()));
        }
    }
    wayland_client::event_created_child!(Probe, window_list::ExtForeignToplevelListV1, [0 => (window_handle::ExtForeignToplevelHandleV1, ())]);
}
impl Dispatch<window_handle::ExtForeignToplevelHandleV1, ()> for Probe {
    fn event(
        state: &mut Self,
        handle: &window_handle::ExtForeignToplevelHandleV1,
        event: window_handle::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            window_handle::Event::Title { title } => {
                if let Some((_, name)) = state.windows.iter_mut().find(|(item, _)| item == handle) {
                    *name = title;
                }
            }
            window_handle::Event::Closed => state.windows.retain(|(item, _)| item != handle),
            _ => {}
        }
    }
}
impl Dispatch<sessions::ExtImageCopyCaptureSessionV1, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &sessions::ExtImageCopyCaptureSessionV1,
        event: sessions::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            sessions::Event::BufferSize { width, height } => state.size = (width, height),
            sessions::Event::DmabufDevice { device } => {
                if let Ok(bytes) = device.try_into() {
                    state.node = DrmNode::from_dev_id(u64::from_ne_bytes(bytes)).ok();
                }
            }
            sessions::Event::DmabufFormat { format, modifiers } => {
                for bytes in modifiers.as_chunks::<8>().0 {
                    state.formats.push((format, u64::from_ne_bytes(*bytes)));
                }
            }
            sessions::Event::Done => state.ready = true,
            sessions::Event::Stopped => state.failure = Some("capture session stopped".into()),
            _ => {}
        }
    }
}
impl Dispatch<frames::ExtImageCopyCaptureFrameV1, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &frames::ExtImageCopyCaptureFrameV1,
        event: frames::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            frames::Event::Ready => state.done = true,
            frames::Event::Failed { reason } => {
                state.failure = Some(format!("capture failed: {reason:?}"))
            }
            _ => {}
        }
    }
}
fn main() -> Result<()> {
    let options = Options::parse();
    ensure!(
        (1..=240).contains(&options.fps) && (1..=36000).contains(&options.frames),
        "invalid frame count or cadence"
    );
    let connection = Connection::connect_to_env()?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut probe = Probe::default();
    connection.display().get_registry(&qh, ());
    queue.roundtrip(&mut probe)?;
    queue.roundtrip(&mut probe)?;
    let source = if let Some(title) = &options.window_title {
        let (handle, _) = probe
            .windows
            .iter()
            .find(|(_, name)| name == title)
            .context("capture window not listed")?;
        probe
            .window_sources
            .as_ref()
            .context("window capture protocol missing")?
            .create_source(handle, &qh, ())
    } else {
        probe
            .sources
            .as_ref()
            .context("output source protocol missing")?
            .create_source(probe.output.as_ref().context("output missing")?, &qh, ())
    };
    let session = probe
        .copies
        .as_ref()
        .context("capture protocol missing")?
        .create_session(
            &source,
            if options.cursor {
                copies::Options::PaintCursors
            } else {
                copies::Options::empty()
            },
            &qh,
            (),
        );
    while !probe.ready {
        queue.blocking_dispatch(&mut probe)?;
        ensure!(probe.failure.is_none(), "{:?}", probe.failure);
    }
    let (width, height) = probe.size;
    ensure!(
        width > 0 && height > 0 && width <= 16384 && height <= 16384,
        "invalid source dimensions"
    );
    let fourcc = Fourcc::Xrgb8888;
    ensure!(
        probe
            .formats
            .contains(&(fourcc as u32, Modifier::Linear.into())),
        "capture lacks linear XRGB DMA-BUF support"
    );
    let path = probe
        .node
        .context("DMA-BUF device missing")?
        .dev_path()
        .context("DRM path missing")?;
    let file = File::options().read(true).write(true).open(path)?;
    let mut allocator = GbmAllocator::new(GbmDevice::new(file)?, GbmBufferFlags::RENDERING);
    let allocation = allocator.create_buffer(width, height, fourcc, &[Modifier::Linear])?;
    let buffer = allocation.export()?;
    let params = probe
        .dma
        .as_ref()
        .context("linux-dmabuf missing")?
        .create_params(&qh, ());
    let modifier = u64::from(buffer.format().modifier);
    for (plane, ((fd, offset), stride)) in buffer
        .handles()
        .zip(buffer.offsets())
        .zip(buffer.strides())
        .enumerate()
    {
        params.add(
            fd,
            plane as u32,
            offset,
            stride,
            (modifier >> 32) as u32,
            modifier as u32,
        );
    }
    let dma_buffer = params.create_immed(
        width as i32,
        height as i32,
        fourcc as u32,
        params::Flags::empty(),
        &qh,
        (),
    );
    params.destroy();
    let mut shm_file = tempfile::tempfile()?;
    shm_file.set_len(u64::from(width) * u64::from(height) * 4)?;
    let pool = probe.shm.as_ref().context("SHM missing")?.create_pool(
        shm_file.as_fd(),
        (width * height * 4) as i32,
        &qh,
        (),
    );
    let shm_buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        (width * 4) as i32,
        wl_shm::Format::Xrgb8888,
        &qh,
        (),
    );
    let invalid_buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Xrgb8888, &qh, ());
    let invalid_frame = session.create_frame(&qh, ());
    invalid_frame.attach_buffer(&invalid_buffer);
    invalid_frame.capture();
    while probe.failure.is_none() {
        queue.blocking_dispatch(&mut probe)?;
        ensure!(!probe.done, "undersized capture buffer was accepted");
    }
    ensure!(
        probe
            .failure
            .take()
            .is_some_and(|failure| failure.contains("BufferConstraints")),
        "unexpected invalid-buffer failure"
    );
    invalid_frame.destroy();
    invalid_buffer.destroy();
    println!("PASS undersized buffer rejected without closing the capture session");
    let mut capture = |destination: &wl_buffer::WlBuffer| -> Result<()> {
        probe.done = false;
        let frame = session.create_frame(&qh, ());
        frame.attach_buffer(destination);
        frame.damage_buffer(0, 0, width as i32, height as i32);
        frame.capture();
        while !probe.done {
            queue.blocking_dispatch(&mut probe)?;
            ensure!(probe.failure.is_none(), "{:?}", probe.failure);
        }
        frame.destroy();
        Ok(())
    };
    capture(&shm_buffer)?;
    let started = Instant::now();
    for index in 0..options.frames {
        capture(&dma_buffer)?;
        let deadline =
            started + Duration::from_secs_f64(f64::from(index + 1) / f64::from(options.fps));
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    }
    if options.compare {
        capture(&shm_buffer)?;
        let mut expected = Vec::new();
        shm_file.seek(SeekFrom::Start(0))?;
        shm_file.read_to_end(&mut expected)?;
        if let Some(color) = &options.corner_rgb {
            ensure!(color.len() == 6, "corner color requires six hex digits");
            let rgb = u32::from_str_radix(color, 16)?;
            let bgra = [
                (rgb & 255) as u8,
                ((rgb >> 8) & 255) as u8,
                ((rgb >> 16) & 255) as u8,
            ];
            for (x, y) in [
                (0, 0),
                (width - 1, 0),
                (0, height - 1),
                (width - 1, height - 1),
            ] {
                let offset = ((y * width + x) * 4) as usize;
                ensure!(
                    expected[offset..offset + 3] == bgra,
                    "window corner at {x},{y} differs from application content: {:?}",
                    &expected[offset..offset + 3]
                );
            }
        }
        buffer.sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::READ)?;
        let mapping = buffer.map_plane(0, DmabufMappingMode::READ)?;
        let stride = buffer.strides().next().context("missing stride")? as usize;
        let row_bytes = width as usize * 4;
        ensure!(
            mapping.length() >= stride * (height as usize - 1) + row_bytes,
            "mapped buffer too short"
        );
        let mut mismatches = 0;
        for row in 0..height as usize {
            // SAFETY: ready completed the compositor write; this private probe
            // exclusively owns the allocation and checked the mapped row bounds.
            let actual = unsafe {
                std::slice::from_raw_parts(mapping.ptr().cast::<u8>().add(row * stride), row_bytes)
            };
            for (actual, expected) in actual.as_chunks::<4>().0.iter().zip(
                expected[row * row_bytes..(row + 1) * row_bytes]
                    .as_chunks::<4>()
                    .0,
            ) {
                if actual[..3] != expected[..3] {
                    mismatches += 1;
                }
            }
        }
        buffer.sync_plane(0, DmabufSyncFlags::END | DmabufSyncFlags::READ)?;
        ensure!(
            mismatches == 0,
            "{mismatches} pixels differ between DMA-BUF and SHM"
        );
        println!("PASS DMA-BUF pixels match SHM at {width}x{height}");
    }
    if let Some(marker) = &options.lifecycle {
        std::fs::write(marker.with_extension("ready"), b"ready")?;
        while probe.size == (width, height) {
            queue.blocking_dispatch(&mut probe)?;
            ensure!(probe.failure.is_none(), "window stopped before resize");
        }
        let (next_width, next_height) = probe.size;
        let resized_file = tempfile::tempfile()?;
        resized_file.set_len(u64::from(next_width) * u64::from(next_height) * 4)?;
        let resized_pool = probe.shm.as_ref().context("SHM missing")?.create_pool(
            resized_file.as_fd(),
            (next_width * next_height * 4) as i32,
            &qh,
            (),
        );
        let resized_buffer = resized_pool.create_buffer(
            0,
            next_width as i32,
            next_height as i32,
            (next_width * 4) as i32,
            wl_shm::Format::Xrgb8888,
            &qh,
            (),
        );
        probe.done = false;
        let frame = session.create_frame(&qh, ());
        frame.attach_buffer(&resized_buffer);
        frame.damage_buffer(0, 0, next_width as i32, next_height as i32);
        frame.capture();
        while !probe.done {
            queue.blocking_dispatch(&mut probe)?;
            ensure!(
                probe.failure.is_none(),
                "resized capture failed: {:?}",
                probe.failure
            );
        }
        frame.destroy();
        resized_buffer.destroy();
        resized_pool.destroy();
        std::fs::write(
            marker.with_extension("resized"),
            format!("{next_width}x{next_height}"),
        )?;
        while probe.failure.is_none() {
            queue.blocking_dispatch(&mut probe)?;
        }
        ensure!(
            probe.failure.as_deref() == Some("capture session stopped"),
            "unexpected close failure: {:?}",
            probe.failure
        );
        queue.roundtrip(&mut probe)?;
        ensure!(
            !probe
                .windows
                .iter()
                .any(|(_, title)| Some(title) == options.window_title.as_ref()),
            "closed window remains listed"
        );
        println!(
            "PASS window resize renegotiated to {next_width}x{next_height}; close stopped capture and removed its identity"
        );
    }
    session.destroy();
    source.destroy();
    dma_buffer.destroy();
    shm_buffer.destroy();
    pool.destroy();
    connection.flush()?;
    println!(
        "PASS {} DMA-BUF frames at requested {} fps in {:.3}s",
        options.frames,
        options.fps,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
