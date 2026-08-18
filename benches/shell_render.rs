use std::time::{Duration, Instant};

use anyhow::Result;
use weld_app::{benchmark, input::GlobalShortcutPlugin};
use weld_core::{
    OutputId,
    host::{CompositionDestination, CompositionOutputRequest},
    input::{InputPosition, RawSeatEvent, RawSeatEventKind},
    server::{
        PendingSurfaceBufferContent, PendingSurfaceBufferUpdate, PendingSurfaceEvent,
        PendingSurfaceEventKind, PendingSurfaceTreeSnapshot,
    },
    surface::{
        LogicalPoint, LogicalSize, SurfaceContentView, SurfaceId, SurfaceInputPlacement,
        SurfaceInputRect, SurfaceLayerId, SurfaceLayerPlacement, SurfaceWindowGeometry,
        WindowDecoration,
    },
};
use weld_float::FloatPlugin;
use weld_ssd::SsdPlugin;
use weld_window::WindowPlugin;
use weld_window_ui::WindowUiPlugin;

const OUTPUT: OutputId = OutputId::new(1);
const CLIENT_SURFACE: SurfaceId = SurfaceId::new(1);
const CLIENT_LAYER: SurfaceLayerId = SurfaceLayerId::new(1);
const CLIENT_WIDTH: u32 = 900;
const CLIENT_HEIGHT: u32 = 600;

fn main() -> Result<()> {
    let frames = environment_usize("WELD_RENDER_BENCH_FRAMES", 600);
    let warmup_frames = environment_usize("WELD_RENDER_BENCH_WARMUP", 30);
    for (mapped_client, retained_client_commit, events_per_frame) in [
        (false, false, 0),
        (false, false, 16),
        (true, false, 0),
        (true, false, 16),
        (true, true, 0),
        (true, true, 16),
    ] {
        run_case(
            frames,
            warmup_frames,
            mapped_client,
            retained_client_commit,
            events_per_frame,
        )?;
    }
    Ok(())
}

fn run_case(
    frames: usize,
    warmup_frames: usize,
    mapped_client: bool,
    retained_client_commit: bool,
    events_per_frame: usize,
) -> Result<()> {
    let (mut shell, adapter) = benchmark::rendering_shell(configure_shell)?;
    if mapped_client {
        map_synthetic_client(&mut shell);
    }
    let events = pointer_events(events_per_frame);
    for frame in 0..warmup_frames {
        enqueue_events(&mut shell, &events);
        if retained_client_commit {
            enqueue_retained_commit(&mut shell);
        }
        shell.advance_main(frame as u32);
        drop(shell.render_outputs(output_request())?);
    }
    shell.wait_for_gpu_for_benchmark()?;

    let mut ingress = Duration::ZERO;
    let mut surface_commit = Duration::ZERO;
    let mut main = Duration::ZERO;
    let mut render_submit = Duration::ZERO;
    let mut gpu_completion = Duration::ZERO;
    for frame in 0..frames {
        ingress += elapsed(|| enqueue_events(&mut shell, &events));
        if retained_client_commit {
            surface_commit += elapsed(|| enqueue_retained_commit(&mut shell));
        }
        main += elapsed(|| {
            shell.advance_main(frame as u32);
        });
        render_submit += elapsed_result(|| {
            drop(shell.render_outputs(output_request())?);
            Ok(())
        })?;
        gpu_completion += elapsed_result(|| shell.wait_for_gpu_for_benchmark())?;
    }

    println!(
        "adapter={:?} backend={:?} device={:?} mapped-client={} retained-client-commit={} pointer-events/frame={} frames={} input-ingress={:.3} surface-ingress={:.3} main={:.3} render-submit={:.3} gpu-wait={:.3} total={:.3} us/frame",
        adapter.name,
        adapter.backend,
        adapter.device_type,
        mapped_client,
        retained_client_commit,
        events_per_frame,
        frames,
        micros_per_frame(ingress, frames),
        micros_per_frame(surface_commit, frames),
        micros_per_frame(main, frames),
        micros_per_frame(render_submit, frames),
        micros_per_frame(gpu_completion, frames),
        micros_per_frame(
            ingress + surface_commit + main + render_submit + gpu_completion,
            frames,
        ),
    );
    Ok(())
}

fn configure_shell(app: &mut bevy::app::App) {
    app.add_plugins((
        WindowPlugin,
        WindowUiPlugin,
        SsdPlugin,
        FloatPlugin,
        GlobalShortcutPlugin,
    ));
}

fn map_synthetic_client(shell: &mut benchmark::AppShell) {
    std::hint::black_box(shell.enqueue_surface_event(PendingSurfaceEvent {
        surface: CLIENT_SURFACE,
        kind: PendingSurfaceEventKind::Created {
            decoration: WindowDecoration::ServerSide,
        },
    }));
    std::hint::black_box(shell.enqueue_surface_event(surface_snapshot(
        PendingSurfaceBufferContent::ShmPixels(vec![
            0;
            CLIENT_WIDTH as usize
                * CLIENT_HEIGHT as usize
                * 4
        ]),
    )));
}

fn enqueue_retained_commit(shell: &mut benchmark::AppShell) {
    std::hint::black_box(
        shell.enqueue_surface_event(surface_snapshot(PendingSurfaceBufferContent::Retained)),
    );
}

fn surface_snapshot(content: PendingSurfaceBufferContent) -> PendingSurfaceEvent {
    let view = SurfaceContentView {
        source_x: 0.0,
        source_y: 0.0,
        source_width: CLIENT_WIDTH as f32,
        source_height: CLIENT_HEIGHT as f32,
        logical_width: CLIENT_WIDTH as f32,
        logical_height: CLIENT_HEIGHT as f32,
    };
    PendingSurfaceEvent {
        surface: CLIENT_SURFACE,
        kind: PendingSurfaceEventKind::TreeSnapshot(PendingSurfaceTreeSnapshot {
            client_mapped: true,
            root: Some(SurfaceLayerPlacement {
                layer: CLIENT_LAYER,
                position: LogicalPoint::ZERO,
                view,
            }),
            window_geometry: Some(SurfaceWindowGeometry {
                origin: LogicalPoint::ZERO,
                view,
            }),
            overlays: Vec::new(),
            inputs: vec![SurfaceInputPlacement {
                layer: CLIENT_LAYER,
                position: LogicalPoint::ZERO,
                regions: vec![SurfaceInputRect {
                    position: LogicalPoint::ZERO,
                    size: LogicalSize::new(CLIENT_WIDTH as f32, CLIENT_HEIGHT as f32),
                }],
            }],
            buffers: vec![PendingSurfaceBufferUpdate {
                layer: CLIENT_LAYER,
                width: CLIENT_WIDTH,
                height: CLIENT_HEIGHT,
                content,
                opaque: true,
            }],
        }),
    }
}

fn pointer_events(count: usize) -> Vec<RawSeatEvent> {
    (0..count)
        .map(|index| {
            RawSeatEvent::new(
                RawSeatEventKind::PointerMotion {
                    position: InputPosition::new(
                        400.0 + index as f64 * 0.25,
                        300.0 + index as f64 * 0.125,
                    ),
                },
                u32::try_from(index).unwrap_or(u32::MAX),
            )
        })
        .collect()
}

fn enqueue_events(shell: &mut benchmark::AppShell, events: &[RawSeatEvent]) {
    for event in events {
        std::hint::black_box(shell.enqueue_input_event(event.clone()));
    }
}

fn output_request() -> Vec<CompositionOutputRequest> {
    vec![CompositionOutputRequest {
        output: OUTPUT,
        destination: CompositionDestination::Owned,
    }]
}

fn elapsed(run: impl FnOnce()) -> Duration {
    let start = Instant::now();
    run();
    start.elapsed()
}

fn elapsed_result(run: impl FnOnce() -> Result<()>) -> Result<Duration> {
    let start = Instant::now();
    run()?;
    Ok(start.elapsed())
}

fn environment_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn micros_per_frame(duration: Duration, frames: usize) -> f64 {
    duration.as_secs_f64() * 1_000_000.0 / frames as f64
}
