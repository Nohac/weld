//! Opt-in offscreen performance fixture using the production native host loop.

mod driver;
mod surface;

pub use crate::renderer::composite::CompositionBlitter as SurfaceBlitter;
pub use surface::SurfaceOnly;

use crate::{
    OutputConfiguration, OutputHead, OutputId, OutputScale, PreparedHost, RenderContext,
    dmabuf::DirectClientBufferAccess,
    input::KeyboardRepeatMode,
    runtime::{
        gpu::{NativeGpu, import_channel},
        native::{NativeRuntime, RuntimeIntegration, RuntimeSetup},
    },
    server::{
        OutputDescriptor, OutputMetrics, ServerOptions, ServerOutputDefinition,
        WaylandClientBridge, client_registration,
    },
    surface::{Extent, LogicalPoint},
};
use anyhow::{Context, Result, ensure};
use calloop::signals::{Signal, Signals};
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::OsString,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};
use weld_client::{
    ClientAdapter, ClientAdapterCommandEnvelope, ClientAdapterRegistration, ClientEventQueue,
    ClientInputEvent, ClientProvenance, ClientRequest, ClientSourceDescriptor, ClientSourceId,
    ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceId, ControlOnlyClientImporter,
    SurfaceBufferChange,
};

pub struct Options {
    pub extent: Extent,
    pub window_extent: Extent,
    pub hz: u32,
    pub input_hz: u32,
    pub warmup: Duration,
    pub duration: Duration,
    pub socket: String,
    pub capture: PathBuf,
    pub command: Vec<OsString>,
}

#[derive(Default, Debug)]
pub struct Report {
    pub elapsed: Duration,
    /// Whole-process user + system CPU, excluding the producer and final capture.
    pub cpu_seconds: f64,
    /// Submitted offscreen compositions; GPU completion is independently bounded.
    pub compositions: u64,
    pub commits: u64,
    pub replaced_before_composition: u64,
    pub dmabuf_buffers: u64,
    pub shm_buffers: u64,
    pub inputs: u64,
    pub max_in_flight: usize,
    pub policy_wall: Duration,
    pub render_wall: Duration,
    /// Latest commit's bridge-drain to CPU submission duration for each surface.
    pub ingress_ages: Vec<Duration>,
}

#[derive(Default)]
struct Observations {
    measuring: bool,
    report: Report,
    pending: HashMap<ClientSurfaceId, Instant>,
}

impl Observations {
    fn record_commit(&mut self, surface: ClientSurfaceId, now: Instant) {
        self.report.commits += 1;
        if self.pending.insert(surface, now).is_some() {
            self.report.replaced_before_composition += 1;
        }
    }
}

#[derive(Clone)]
pub struct Results(Rc<RefCell<Observations>>);
impl Results {
    pub fn take(&self) -> Report {
        std::mem::take(&mut self.0.borrow_mut().report)
    }
}

struct Observe(Rc<RefCell<Observations>>);
impl ClientAdapter for Observe {
    fn drain_events(&mut self, _: &mut ClientEventQueue) {}
    fn apply_request(&mut self, _: ClientRequest) {}
    fn apply_input(&mut self, _: ClientInputEvent) {}
    fn apply_command(&mut self, _: ClientAdapterCommandEnvelope) {}
    fn host_focus_lost(&mut self, _: u32) {}
    fn observe_event(&mut self, event: &ClientSurfaceEvent) {
        let mut observations = self.0.borrow_mut();
        if !observations.measuring {
            return;
        }
        match &event.kind {
            ClientSurfaceEventKind::Commit(commit) if commit.mapped => {
                observations.record_commit(event.surface, Instant::now());
                for buffer in &commit.buffers {
                    if let SurfaceBufferChange::Replaced { buffer, .. } = &buffer.change {
                        match buffer.access::<DirectClientBufferAccess>() {
                            Some(DirectClientBufferAccess::Dmabuf(_)) => {
                                observations.report.dmabuf_buffers += 1
                            }
                            Some(DirectClientBufferAccess::Shm(_)) => {
                                observations.report.shm_buffers += 1
                            }
                            None => {}
                        }
                    }
                }
            }
            ClientSurfaceEventKind::Destroyed => {
                observations.pending.remove(&event.surface);
            }
            _ => {}
        }
    }
}

/// Prepare one real GPU and native client host with a paced offscreen consumer.
pub fn prepare(options: Options) -> Result<(PreparedHost, Results)> {
    ensure!(
        (1..=240).contains(&options.hz),
        "output rate must be 1..240 Hz"
    );
    ensure!(options.input_hz <= 2000, "input rate exceeds 2000 Hz");
    ensure!(
        !options.duration.is_zero() && options.duration <= Duration::from_secs(120),
        "measurement must be 0..120 seconds"
    );
    ensure!(
        options.warmup <= Duration::from_secs(30),
        "warmup exceeds thirty seconds"
    );
    let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM])?;
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(descriptor);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))?;
    let info = adapter.get_info();
    ensure!(
        info.device_type != wgpu::DeviceType::Cpu,
        "CPU adapter would invalidate GPU comparison"
    );
    println!(
        "ADAPTER name={:?} backend={:?} device={:?}",
        info.name, info.backend, info.device_type
    );
    let gpu = NativeGpu::request(&adapter, "weld paced benchmark")?;
    ensure!(
        gpu.capabilities.is_some(),
        "benchmark requires direct DMA-BUF import"
    );
    let (dmabuf, releases) = import_channel(gpu.sources.clone(), gpu.capabilities.clone());
    let bridge = WaylandClientBridge::default();
    let observations = Rc::new(RefCell::new(Observations::default()));
    let results = Results(observations.clone());
    let registrations = vec![
        client_registration(bridge.clone(), dmabuf.clone()),
        ClientAdapterRegistration::new(
            ClientSourceDescriptor::new(ClientSourceId::new(901), ClientProvenance::Relocated),
            Observe(observations.clone()),
            ControlOnlyClientImporter,
        ),
    ];
    let configuration = OutputConfiguration::new(
        OutputId::new(1),
        options.extent,
        OutputScale::default(),
        LogicalPoint::ZERO,
        true,
        None,
    )?;
    let context = RenderContext {
        instance,
        adapter,
        device: gpu.device,
        queue: gpu.queue,
        dmabuf,
        output_heads: vec![OutputHead::new(configuration.id(), "benchmark", None)],
        outputs: vec![configuration],
        composition_format: wgpu::TextureFormat::Bgra8UnormSrgb,
    };
    let device = context.device.clone();
    let queue = context.queue.clone();
    let prepared = PreparedHost::new(context, registrations, move |app, adapters, wakes| {
        let started = Instant::now();
        let interval = Duration::from_secs_f64(1.0 / f64::from(options.hz));
        let (notifier, wake) = crate::host::client_runtime_notifier()?;
        let mut wakes = wakes;
        wakes.push(wake);
        let mut runtime = NativeRuntime::prepare(RuntimeSetup {
            xwayland: false,
            server: ServerOptions {
                started_at: started,
                seat_name: "benchmark",
                outputs: vec![ServerOutputDefinition {
                    id: OutputId::new(1),
                    descriptor: OutputDescriptor::nested(),
                    primary: true,
                    logical_position: (0, 0),
                    metrics: OutputMetrics::new(
                        options.extent.width,
                        options.extent.height,
                        OutputScale::default(),
                    )?
                    .with_refresh_millihertz(i32::try_from(options.hz * 1000)?)?,
                }],
                dmabuf_capabilities: gpu.capabilities.as_ref(),
                dmabuf_sources: gpu.sources,
                socket_name: Some(&options.socket),
                keyboard_repeat_mode: KeyboardRepeatMode::Client,
                initial_toplevel_size: Some(options.window_extent),
            },
            releases,
            bridge,
            adapters,
            wakes,
            signals,
            shutdown_event: || (),
        })?;
        runtime
            .state
            .children
            .spawn_requested(&runtime.state.data.server, &options.command)?;
        let driver = driver::Driver::new(options, observations, device, queue, notifier)?;
        runtime
            .run(
                RuntimeIntegration::Native {
                    application: app,
                    driver: Box::new(driver),
                },
                interval,
            )
            .context("paced offscreen benchmark failed")
    });
    Ok((prepared, results))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacements_are_counted_per_surface_between_compositions() {
        let mut observations = Observations::default();
        let first = ClientSurfaceId::for_test(1);
        let second = ClientSurfaceId::for_test(2);
        let now = Instant::now();
        observations.record_commit(first, now);
        observations.record_commit(second, now);
        observations.record_commit(first, now + Duration::from_millis(1));
        assert_eq!(observations.report.commits, 3);
        assert_eq!(observations.report.replaced_before_composition, 1);
        assert_eq!(observations.pending.len(), 2);
        assert_eq!(observations.pending[&first], now + Duration::from_millis(1));
        observations.pending.clear();
        observations.record_commit(first, now + Duration::from_millis(17));
        assert_eq!(observations.report.replaced_before_composition, 1);
    }
    #[test]
    fn opaque_client_shader_validates() {
        let shader =
            wgpu::naga::front::wgsl::parse_str(include_str!("surface.wgsl")).expect("shader");
        wgpu::naga::valid::Validator::new(
            wgpu::naga::valid::ValidationFlags::all(),
            wgpu::naga::valid::Capabilities::all(),
        )
        .validate(&shader)
        .expect("valid shader");
    }
}
