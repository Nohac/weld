//! Headless measurements for Weld's production application boundary.

use std::{collections::VecDeque, time::Duration, time::Instant};

use anyhow::{Context, Result};
use bevy::{
    app::{App, First, Last, Plugin, PostUpdate, PreUpdate, TerminalCtrlCHandlerPlugin, Update},
    asset::AssetApp,
    camera::{Camera, Camera2d, ManualTextureViewHandle, NormalizedRenderTarget, RenderTarget},
    ecs::entity::Entity,
    input::InputPlugin,
    log::LogPlugin,
    picking::pointer::PointerInput,
    prelude::{DefaultPlugins, IsDefaultUiCamera, MinimalPlugins, PluginGroup, With},
    render::RenderPlugin,
    shader::Shader,
    window::{ExitCondition, WindowPlugin},
};
use weld_core::{
    OutputConfiguration, OutputHead, OutputId, OutputScale,
    dmabuf::DmabufContext,
    host::RenderContext,
    input::{InputPosition, RawSeatEvent, RawSeatEventKind},
    surface::{Extent, LogicalPoint},
};

use crate::input::{InputBridgePlugin, InputOutputTarget, enqueue_raw_input_batch};
use crate::output::{PrimaryOutput, RendersOutput};
use crate::shell::{WeldAppPlugin, configure_rendering};

pub use crate::shell::AppShell;

/// Aggregated wall-clock timings from one headless input workload.
#[derive(Clone, Copy, Debug, Default)]
pub struct InputBenchmarkReport {
    pub frames: u64,
    pub events_per_frame: usize,
    pub ingress: Duration,
    pub first: Duration,
    pub pre_update: Duration,
    pub update: Duration,
    pub post_update: Duration,
    pub last: Duration,
    pub full_update: Duration,
    pub production_update: Duration,
}

/// Per-schedule timings for a caller-configured headless application.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScheduleBenchmarkReport {
    pub frames: u64,
    pub events_per_frame: usize,
    pub ingress: Duration,
    pub first: Duration,
    pub pre_update: Duration,
    pub update: Duration,
    pub post_update: Duration,
    pub last: Duration,
}

impl InputBenchmarkReport {
    pub fn print(self) {
        println!(
            "events/frame={} frames={} ingress={:.3} us/frame First={:.3} us/frame PreUpdate={:.3} us/frame Update={:.3} us/frame PostUpdate={:.3} us/frame Last={:.3} us/frame minimal App::update={:.3} us/frame production-main App::update={:.3} us/frame",
            self.events_per_frame,
            self.frames,
            micros_per_frame(self.ingress, self.frames),
            micros_per_frame(self.first, self.frames),
            micros_per_frame(self.pre_update, self.frames),
            micros_per_frame(self.update, self.frames),
            micros_per_frame(self.post_update, self.frames),
            micros_per_frame(self.last, self.frames),
            micros_per_frame(self.full_update, self.frames),
            micros_per_frame(self.production_update, self.frames),
        );
    }
}

/// Run the same empty-background pointer workload through individual schedules
/// and through the complete headless Bevy main schedule.
pub fn run(frames: u64, events_per_frame: usize) -> InputBenchmarkReport {
    let frames = frames.max(1);
    let events_per_frame = events_per_frame.max(1);
    let events = pointer_events(events_per_frame);
    let mut scheduled = benchmark_app();
    prepare(&mut scheduled);
    warm_up(&mut scheduled, &events);

    let mut report = InputBenchmarkReport {
        frames,
        events_per_frame,
        ..Default::default()
    };
    for _ in 0..frames {
        let mut batch = VecDeque::from(events.clone());
        report.ingress += elapsed(|| {
            enqueue_raw_input_batch(scheduled.world_mut(), &mut batch);
        });
        report.first += elapsed(|| scheduled.world_mut().run_schedule(First));
        report.pre_update += elapsed(|| scheduled.world_mut().run_schedule(PreUpdate));
        report.update += elapsed(|| scheduled.world_mut().run_schedule(Update));
        report.post_update += elapsed(|| scheduled.world_mut().run_schedule(PostUpdate));
        report.last += elapsed(|| scheduled.world_mut().run_schedule(Last));
        scheduled.world_mut().clear_trackers();
    }

    let mut complete = benchmark_app();
    prepare(&mut complete);
    warm_up(&mut complete, &events);
    for _ in 0..frames {
        let mut batch = VecDeque::from(events.clone());
        enqueue_raw_input_batch(complete.world_mut(), &mut batch);
        report.full_update += elapsed(|| complete.update());
    }

    let mut production = production_app();
    prepare(&mut production);
    warm_up(&mut production, &events);
    for _ in 0..frames {
        let mut batch = VecDeque::from(events.clone());
        enqueue_raw_input_batch(production.world_mut(), &mut batch);
        report.production_update += elapsed(|| production.update());
    }
    report
}

/// Measure complete main-schedule updates for a caller-configured headless app.
pub fn run_app_updates(mut app: App, frames: u64, events_per_frame: usize) -> (App, Duration) {
    let frames = frames.max(1);
    let events = pointer_events(events_per_frame.max(1));
    prepare(&mut app);
    warm_up(&mut app, &events);
    let mut duration = Duration::ZERO;
    for _ in 0..frames {
        let mut batch = VecDeque::from(events.clone());
        enqueue_raw_input_batch(app.world_mut(), &mut batch);
        duration += elapsed(|| app.update());
    }
    (app, duration)
}

/// Measure the main schedule stages independently for a caller-configured app.
pub fn run_app_schedules(
    mut app: App,
    frames: u64,
    events_per_frame: usize,
) -> (App, ScheduleBenchmarkReport) {
    let frames = frames.max(1);
    let events_per_frame = events_per_frame.max(1);
    let events = pointer_events(events_per_frame);
    prepare(&mut app);
    warm_up(&mut app, &events);
    let mut report = ScheduleBenchmarkReport {
        frames,
        events_per_frame,
        ..Default::default()
    };
    for _ in 0..frames {
        let mut batch = VecDeque::from(events.clone());
        report.ingress += elapsed(|| {
            enqueue_raw_input_batch(app.world_mut(), &mut batch);
        });
        report.first += elapsed(|| app.world_mut().run_schedule(First));
        report.pre_update += elapsed(|| app.world_mut().run_schedule(PreUpdate));
        report.update += elapsed(|| app.world_mut().run_schedule(Update));
        report.post_update += elapsed(|| app.world_mut().run_schedule(PostUpdate));
        report.last += elapsed(|| app.world_mut().run_schedule(Last));
        app.world_mut().clear_trackers();
    }
    (app, report)
}

fn benchmark_app() -> App {
    let configuration = OutputConfiguration::new(
        OutputId::new(1),
        Extent::new(1920, 1080),
        OutputScale::default(),
        LogicalPoint::ZERO,
        true,
        None,
    )
    .expect("benchmark output configuration should be valid");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(InputPlugin)
        .add_message::<PointerInput>()
        .add_plugins(InputBridgePlugin::new(vec![InputOutputTarget {
            configuration,
            target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
        }]));
    app.init_schedule(Update)
        .init_schedule(PostUpdate)
        .init_schedule(Last);
    app
}

/// Construct Weld's normal Bevy plugin stack without a render sub-app.
pub fn production_app() -> App {
    let configuration = output_configuration();
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                ..Default::default()
            })
            .add_before::<RenderPlugin>(HeadlessRenderPrerequisitesPlugin)
            .disable::<RenderPlugin>()
            .disable::<LogPlugin>()
            .disable::<TerminalCtrlCHandlerPlugin>(),
    )
    .add_plugins(
        WeldAppPlugin::new(
            vec![configuration],
            vec![OutputHead::new(configuration.id(), "benchmark", None)],
        )
        .expect("benchmark Weld plugin should accept one primary output"),
    );
    let output = app
        .world_mut()
        .query_filtered::<Entity, With<PrimaryOutput>>()
        .single(app.world())
        .expect("benchmark app should contain one primary output");
    app.world_mut().spawn((
        Camera2d,
        Camera::default(),
        RenderTarget::TextureView(ManualTextureViewHandle(1)),
        IsDefaultUiCamera,
        RendersOutput(output),
    ));
    app
}

/// Construct the real [`AppShell`] render bridge against a headless Vulkan
/// device, then allow the caller to install the policy plugins under test.
///
/// This omits calloop, Smithay protocol dispatch, and physical presentation.
/// Everything from host input ingress through Bevy extraction and composition
/// submission is the same path used by the nested and DRM backends.
pub fn rendering_shell(configure: impl FnOnce(&mut App)) -> Result<(AppShell, wgpu::AdapterInfo)> {
    let context = headless_render_context()?;
    let adapter_info = context.adapter.get_info();
    let configuration = context
        .outputs
        .first()
        .copied()
        .context("benchmark render context contains no output")?;
    let mut app = App::new();
    configure_rendering(&mut app, &context);
    app.add_plugins(WeldAppPlugin::new(
        vec![configuration],
        vec![OutputHead::new(configuration.id(), "benchmark", None)],
    )?);
    configure(&mut app);
    Ok((AppShell::new(app, context)?, adapter_info))
}

fn headless_render_context() -> Result<RenderContext> {
    let mut instance_descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    instance_descriptor.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(instance_descriptor);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .context("no Vulkan adapter is available for the headless render benchmark")?;
    let descriptor = wgpu::DeviceDescriptor {
        label: Some("weld headless render benchmark device"),
        ..Default::default()
    };
    let (device, queue) = pollster::block_on(adapter.request_device(&descriptor))
        .context("failed to create the headless render benchmark device")?;
    let configuration = output_configuration();
    let dmabuf = DmabufContext::for_headless_benchmark(&device);
    Ok(RenderContext {
        instance,
        adapter,
        device,
        queue,
        dmabuf,
        output_heads: vec![OutputHead::new(configuration.id(), "benchmark", None)],
        outputs: vec![configuration],
        composition_format: wgpu::TextureFormat::Bgra8UnormSrgb,
    })
}

struct HeadlessRenderPrerequisitesPlugin;

impl Plugin for HeadlessRenderPrerequisitesPlugin {
    fn build(&self, app: &mut App) {
        app.init_asset::<Shader>();
    }
}

fn output_configuration() -> OutputConfiguration {
    OutputConfiguration::new(
        OutputId::new(1),
        Extent::new(1920, 1080),
        OutputScale::default(),
        LogicalPoint::ZERO,
        true,
        None,
    )
    .expect("benchmark output configuration should be valid")
}

fn warm_up(app: &mut App, events: &[RawSeatEvent]) {
    for _ in 0..100 {
        let mut batch = VecDeque::from(events.to_vec());
        enqueue_raw_input_batch(app.world_mut(), &mut batch);
        app.update();
    }
}

fn prepare(app: &mut App) {
    app.finish();
    app.cleanup();
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

fn elapsed(run: impl FnOnce()) -> Duration {
    let start = Instant::now();
    run();
    start.elapsed()
}

fn micros_per_frame(duration: Duration, frames: u64) -> f64 {
    duration.as_secs_f64() * 1_000_000.0 / frames as f64
}
