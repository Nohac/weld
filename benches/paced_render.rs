use anyhow::{Context, Result, ensure};
use bevy::ecs::template::template;
use bevy::{
    app::PreUpdate,
    ecs::{
        component::Component,
        query::{Added, With},
        system::{Commands, Query},
    },
    prelude::{Node, PositionType, px},
    scene::{CommandsSceneExt, bsn},
};
use clap::{Parser, ValueEnum};
use std::{path::PathBuf, time::Duration};
use weld_app::{
    benchmark::shell_for_host,
    surface::{ClientSurface, MappedSurface, SurfaceNode, SurfaceView},
};
use weld_core::{
    benchmark::{Options, SurfaceOnly, prepare},
    surface::Extent,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Mode {
    Surface,
    Minimal,
    Master,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Decorations {
    Client,
    Server,
}
#[derive(Parser)]
struct Args {
    #[arg(long, value_enum, default_value_t = Mode::Master)]
    mode: Mode,
    #[arg(long, default_value_t = 1920)]
    width: u32,
    #[arg(long, default_value_t = 1080)]
    height: u32,
    #[arg(long, default_value_t = 60)]
    hz: u32,
    #[arg(long, default_value_t = 120)]
    producer_hz: u32,
    #[arg(long, default_value_t = 0)]
    input_hz: u32,
    #[arg(long, default_value_t = 1)]
    windows: u32,
    #[arg(long, value_enum, default_value_t = Decorations::Client)]
    decorations: Decorations,
    #[arg(long, default_value_t = 10)]
    seconds: u64,
    #[arg(long, default_value_t = 3)]
    warmup: u64,
    #[arg(long, requires = "run_dir")]
    producer: Option<PathBuf>,
    #[arg(long, requires = "producer")]
    run_dir: Option<PathBuf>,
    #[arg(long, default_value = "scripts/profiling/bench.sway.config")]
    config: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse_from(std::env::args_os().filter(|arg| arg != "--bench"));
    let Some(producer) = args.producer else {
        println!(
            "SKIP paced_render: use python3 scripts/profiling/paced-render to build the EGL producer and run the benchmark"
        );
        return Ok(());
    };
    let run_dir = args.run_dir.context("--producer requires --run-dir")?;
    ensure!((1..=8).contains(&args.windows), "window count must be 1..8");
    ensure!(
        (1..=1000).contains(&args.producer_hz),
        "producer rate must be 1..1000"
    );
    ensure!(
        (64..=8192).contains(&args.width) && (64..=8192).contains(&args.height),
        "output dimensions must be 64..8192"
    );
    std::fs::create_dir_all(&run_dir)?;
    let socket = run_dir.canonicalize()?.join("wayland");
    let (prepared, results) = prepare(Options {
        extent: Extent::new(args.width, args.height),
        window_extent: Extent::new(args.width / args.windows, args.height),
        hz: args.hz,
        input_hz: args.input_hz,
        warmup: Duration::from_secs(args.warmup),
        duration: Duration::from_secs(args.seconds),
        capture: run_dir.join("capture.png"),
        socket: socket
            .to_str()
            .context("socket path must be UTF-8")?
            .to_owned(),
        command: vec![
            producer.canonicalize()?.into_os_string(),
            args.producer_hz.to_string().into(),
            args.windows.to_string().into(),
            (args.seconds + args.warmup + 15).to_string().into(),
            match args.decorations {
                Decorations::Client => "client",
                Decorations::Server => "server",
            }
            .into(),
        ],
    })?;
    let (context, runtime, registrations) = prepared.into_parts();
    let (adapters, importers): (Vec<_>, Vec<_>) = registrations
        .into_iter()
        .map(|registration| {
            let parts = registration.into_parts();
            (parts.runtime, parts.importer)
        })
        .unzip();
    println!(
        "CASE mode={:?} output={}x{} output_hz={} producer_hz={} windows={} input_hz={} decorations={:?}",
        args.mode,
        args.width,
        args.height,
        args.hz,
        args.producer_hz,
        args.windows,
        args.input_hz,
        args.decorations
    );
    match args.mode {
        Mode::Surface => runtime.run(SurfaceOnly::new(context)?, adapters, Vec::new())?,
        mode => {
            let mut configured = Ok(());
            let shell = shell_for_host(context, importers, |app| match mode {
                Mode::Master => configured = weldwm::benchmark::configure(app, &args.config),
                Mode::Minimal => {
                    app.add_systems(PreUpdate, mount_surfaces);
                }
                Mode::Surface => {}
            })?;
            configured?;
            runtime.run(shell, adapters, Vec::new())?;
        }
    }
    let mut report = results.take();
    report.ingress_ages.sort();
    let percentile = |fraction: f64| {
        report
            .ingress_ages
            .get(((report.ingress_ages.len().saturating_sub(1)) as f64 * fraction) as usize)
            .map_or(0.0, Duration::as_secs_f64)
            * 1000.0
    };
    let elapsed = report.elapsed.as_secs_f64();
    let frames = report.compositions.max(1) as f64;
    println!(
        "RESULT mode={:?} decorations={:?} wall_s={:.3} cpu_pct={:.3} cpu_ms_per_frame={:.3} compositions={} composition_hz={:.3} commits={} commit_hz={:.3} ingress_replaced={} dmabuf={} shm={} inputs={} max_in_flight={} policy_us_per_frame={:.3} render_us_per_frame={:.3} ingress_age_p50_ms={:.3} ingress_age_p95_ms={:.3}",
        args.mode,
        args.decorations,
        elapsed,
        report.cpu_seconds / elapsed * 100.0,
        report.cpu_seconds / frames * 1000.0,
        report.compositions,
        frames / elapsed,
        report.commits,
        report.commits as f64 / elapsed,
        report.replaced_before_composition,
        report.dmabuf_buffers,
        report.shm_buffers,
        report.inputs,
        report.max_in_flight,
        report.policy_wall.as_secs_f64() / frames * 1e6,
        report.render_wall.as_secs_f64() / frames * 1e6,
        percentile(0.5),
        percentile(0.95)
    );
    Ok(())
}

#[derive(Component, Clone, Default)]
struct BenchSurface;

fn mount_surfaces(
    mut commands: Commands,
    surfaces: Query<(&ClientSurface, &MappedSurface), Added<MappedSurface>>,
    mounted: Query<(), With<BenchSurface>>,
) {
    for (index, (client, mapped)) in (mounted.iter().count()..).zip(&surfaces) {
        let node = Node {
            position_type: PositionType::Absolute,
            left: px(index as f32 * mapped.logical_size.x),
            top: px(0.0),
            width: px(mapped.logical_size.x),
            height: px(mapped.logical_size.y),
            ..Default::default()
        };
        let surface = client.surface;
        commands.spawn_scene(bsn! { (BenchSurface template(move |_| Ok(SurfaceNode { surface, view: SurfaceView::WindowGeometry }))) }).insert(node);
    }
}
