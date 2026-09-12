//! Presentation-free Wayland session hosting (`--backend headless`).
//!
//! Unlike Weld's offscreen-render benchmarks, this host does not compose a
//! desktop, allocate a render target, or require Bevy. A GPU is optional and is
//! used only for the ordinary native client-buffer import capability. Adapters
//! consume client events independently of virtual frame-callback pacing.

use std::{
    ffi::OsString,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use calloop::{
    EventLoop, channel,
    signals::{Signal, Signals},
};
use smithay::{
    output::{PhysicalProperties, Subpixel},
    reexports::wayland_server::Display,
};
use tracing::{info, warn};
use weld_client::{ClientAdapterRegistration, ClientEventQueue, ClientRuntime, Extent};

use crate::{
    OutputId, OutputScale,
    dmabuf::{
        DmabufCapabilities, DmabufContext, DmabufSourceCache, ExternalDmabufCapabilities,
        request_weld_device,
    },
    host::{ClientRuntimeWakeSource, register_client_wake_sources},
    input::{KeyboardRepeatMode, LegacyKeyRepeat},
    runtime::{ChildProcesses, LoopData, server_mut, service_client_adapters},
    server::{
        OutputDescriptor, OutputMetrics, ServerOptions, ServerOutputDefinition, ServerState,
        WaylandClientBridge, client_registration,
    },
};

const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);

/// Validated virtual output and startup window policy. Window sizes are logical
/// pixels; output extent is physical pixels. Neither is an encoder resolution.
/// The output describes a virtual display, not a bound on window geometry.
pub struct SessionHostConfig {
    output: OutputMetrics,
    initial_window_size: Extent,
    frame_interval: Duration,
    socket_name: Option<String>,
    client: Vec<OsString>,
    keyboard_repeat: KeyboardRepeatMode,
    legacy_repeat: LegacyKeyRepeat,
}

impl SessionHostConfig {
    /// Creates a virtual output with a bounded 1..240 Hz callback cadence.
    /// Extents are bounded to 8192 per axis; native client constraints may still
    /// require a different initial window size.
    pub fn new(
        output: Extent,
        scale: OutputScale,
        refresh_hz: u32,
        initial_window_size: Extent,
    ) -> Result<Self> {
        ensure!(
            (1..=240).contains(&refresh_hz),
            "headless refresh must be between 1 and 240 Hz"
        );
        ensure!(
            output.width <= 8192 && output.height <= 8192,
            "headless output exceeds 8192 pixels per axis"
        );
        ensure!(
            (1..=8192).contains(&initial_window_size.width)
                && (1..=8192).contains(&initial_window_size.height),
            "initial window size must be between 1 and 8192 logical pixels per axis"
        );
        let output = OutputMetrics::new(output.width, output.height, scale)?
            .with_refresh_millihertz(i32::try_from(refresh_hz * 1000)?)?;
        Ok(Self {
            output,
            initial_window_size,
            frame_interval: Duration::from_secs_f64(1.0 / f64::from(refresh_hz)),
            socket_name: None,
            client: Vec::new(),
            keyboard_repeat: KeyboardRepeatMode::Client,
            legacy_repeat: LegacyKeyRepeat::default(),
        })
    }

    /// Selects the session socket; `None` uses core's ordinary default name.
    pub fn socket_name(mut self, name: Option<String>) -> Self {
        self.socket_name = name;
        self
    }

    /// Launches one command on this session's socket when [`SessionHost::run`]
    /// starts. Other clients may connect using the same private socket.
    pub fn launch(mut self, command: Vec<OsString>) -> Self {
        self.client = command;
        self
    }

    /// Sets repeat ownership without requiring an application/input plugin.
    pub fn keyboard_repeat(mut self, mode: KeyboardRepeatMode, legacy: LegacyKeyRepeat) -> Self {
        self.keyboard_repeat = mode;
        self.legacy_repeat = legacy;
        self
    }
}

/// A same-thread Wayland host ready for adapter registration, without local
/// presentation. Preparation and execution must occur on the process main
/// thread so newly created GPU/network workers inherit the shutdown signal mask.
pub struct SessionHost {
    event_loop: EventLoop<'static, LoopData<()>>,
    data: LoopData<()>,
    clients: ClientRuntime,
    context: DmabufContext,
    config: SessionHostConfig,
    _gpu: Option<ImportGpu>,
}

impl SessionHost {
    /// Opens the virtual Wayland host and optional GPU imports on the main thread.
    pub fn prepare(config: SessionHostConfig) -> Result<Self> {
        let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM])
            .context("failed to initialize process signal handling")?;
        let started_at = Instant::now();
        let gpu = ImportGpu::open().map(Some).unwrap_or_else(|error| {
            warn!(%error, "headless GPU import unavailable; serving SHM clients only");
            None
        });
        let sources = gpu
            .as_ref()
            .map_or_else(DmabufSourceCache::unavailable, |gpu| {
                DmabufSourceCache::new(&gpu.device)
            });
        let capabilities = gpu.as_ref().and_then(|gpu| gpu.capabilities.clone());
        let (release_sender, release_source) = channel::channel();
        let context = DmabufContext::new(release_sender, sources.clone(), capabilities.clone());
        let event_loop = EventLoop::try_new().context("failed to create headless event loop")?;
        let bridge = WaylandClientBridge::default();
        let mut clients = ClientRuntime::default();
        clients.register(
            client_registration(bridge.clone(), context.clone())
                .into_parts()
                .runtime,
        )?;
        let display = Display::<ServerState>::new().context("failed to create Wayland display")?;
        let mut server = ServerState::new(
            &event_loop.handle(),
            display,
            release_source,
            bridge,
            server_mut::<()>,
            ServerOptions {
                started_at,
                seat_name: "weld-seat0",
                outputs: vec![ServerOutputDefinition {
                    id: OutputId::new(1),
                    descriptor: OutputDescriptor {
                        name: "weld-headless".to_owned(),
                        physical_properties: PhysicalProperties {
                            size: (0, 0).into(),
                            subpixel: Subpixel::Unknown,
                            make: "Weld".to_owned(),
                            model: "Virtual".to_owned(),
                            serial_number: "session".to_owned(),
                        },
                    },
                    metrics: config.output,
                    logical_position: (0, 0),
                    primary: true,
                }],
                dmabuf_capabilities: capabilities.as_ref(),
                dmabuf_sources: sources,
                socket_name: config.socket_name.as_deref(),
                keyboard_repeat_mode: config.keyboard_repeat,
                initial_toplevel_size: Some(config.initial_window_size),
            },
        )?;
        server.set_legacy_key_repeat(config.legacy_repeat);
        event_loop
            .handle()
            .insert_source(signals, |event, _, data| {
                data.events.push_back(());
                tracing::debug!(signal = ?event.signal(), "received headless shutdown signal");
            })
            .context("failed to register process signals")?;
        Ok(Self {
            event_loop,
            data: LoopData::new(server),
            clients,
            context,
            config,
            _gpu: gpu,
        })
    }

    /// Adds a source/relay adapter; no image importer is needed without a local
    /// presenter. Runtime registration still enforces source namespace uniqueness.
    pub fn add_client_adapter(&mut self, adapter: ClientAdapterRegistration) -> Result<&mut Self> {
        self.clients.register(adapter.into_parts().runtime)?;
        Ok(self)
    }

    /// Registers adapter readiness with the same native event loop as Wayland.
    pub fn add_client_wake_source(&mut self, source: ClientRuntimeWakeSource) -> Result<&mut Self> {
        register_client_wake_sources(&self.event_loop.handle(), vec![source])?;
        Ok(self)
    }

    /// Clones the native import/lease capability without exposing the GPU device.
    pub fn dmabuf_context(&self) -> DmabufContext {
        self.context.clone()
    }

    /// Absence is valid for a SHM-only host, not proof of codec support.
    pub fn external_dmabuf_capabilities(&self) -> Result<Option<ExternalDmabufCapabilities>> {
        self.context.external_imports()
    }

    /// Serves clients until SIGINT/SIGTERM, even with no connected applications.
    /// Closing the display disconnects clients; this is not a process supervisor
    /// and does not kill arbitrary descendants of the launched command.
    pub fn run(mut self) -> Result<()> {
        let mut children = ChildProcesses::default();
        children.spawn_requested(&self.data.server, &self.config.client)?;
        let mut events = ClientEventQueue::default();
        let mut invalid_events = Vec::new();
        let mut invalid_effects = Vec::new();
        let mut clock = CallbackClock::new(self.config.frame_interval);
        info!(socket = ?self.data.server.socket_name, width = self.config.output.physical_width(), height = self.config.output.physical_height(), scale = self.config.output.scale_factor(), "Weld headless session is ready");
        loop {
            let timeout = clock.timeout(
                Instant::now(),
                self.data.server.has_pending_frame_callbacks(),
            );
            self.event_loop
                .dispatch(Some(timeout), &mut self.data)
                .context("headless calloop dispatch failed")?;
            if !self.data.events.is_empty() {
                break;
            }
            service_client_adapters(
                &mut self.data.server,
                &mut self.clients,
                &mut events,
                &mut invalid_events,
                &mut invalid_effects,
                |_| {
                    // No local renderer acquires GPU uses in this host.
                },
            );
            // Adapters observed the commits during drain. Without a local
            // presenter, no extra consumer should hold their buffer leases.
            while let Some(event) = events.pop_front() {
                drop(event);
            }
            let now = Instant::now();
            if self.data.server.has_pending_frame_callbacks() && clock.timeout(now, true).is_zero()
            {
                let frame = self.data.server.stage_frame_callbacks();
                self.data.server.complete_frame_callbacks(frame);
                clock.completed(now);
            }
            self.data.server.flush_clients();
            children.reap();
        }
        Ok(())
    }
}

struct ImportGpu {
    _instance: wgpu::Instance,
    _adapter: wgpu::Adapter,
    device: wgpu::Device,
    _queue: wgpu::Queue,
    capabilities: Option<DmabufCapabilities>,
}

impl ImportGpu {
    fn open() -> Result<Self> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        descriptor.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(descriptor);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .context("no Vulkan adapter for headless client imports")?;
        let (device, queue, capabilities) = request_weld_device(&adapter, "Weld session imports")?;
        Ok(Self {
            _instance: instance,
            _adapter: adapter,
            device,
            _queue: queue,
            capabilities,
        })
    }
}

struct CallbackClock {
    interval: Duration,
    last: Option<Instant>,
}

impl CallbackClock {
    const fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
        }
    }

    fn timeout(&self, now: Instant, pending: bool) -> Duration {
        if !pending {
            return MAINTENANCE_INTERVAL;
        }
        self.last.map_or(Duration::ZERO, |last| {
            (last + self.interval).saturating_duration_since(now)
        })
    }

    fn completed(&mut self, now: Instant) {
        self.last = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_output_and_window_coordinates_are_independent() {
        let config = SessionHostConfig::new(
            Extent::new(1920, 1080),
            OutputScale::new(1.5).expect("scale"),
            90,
            Extent::new(960, 640),
        )
        .expect("configuration");
        assert_eq!(config.output.physical_width(), 1920);
        assert_eq!(config.output.scale_factor(), 1.5);
        assert_eq!(config.initial_window_size, Extent::new(960, 640));
        assert_eq!(config.frame_interval, Duration::from_secs_f64(1.0 / 90.0));
        assert_eq!(config.keyboard_repeat, KeyboardRepeatMode::Client);
        let larger_window = SessionHostConfig::new(
            Extent::new(640, 480),
            OutputScale::default(),
            60,
            Extent::new(960, 640),
        )
        .expect("window is not clamped to the virtual display");
        assert_eq!(larger_window.initial_window_size, Extent::new(960, 640));
    }

    #[test]
    fn headless_refresh_and_initial_policy_are_bounded() {
        let output = Extent::new(1920, 1080);
        for refresh in [0, 241, u32::MAX] {
            assert!(
                SessionHostConfig::new(output, OutputScale::default(), refresh, output).is_err()
            );
        }
        assert!(
            SessionHostConfig::new(output, OutputScale::default(), 60, Extent::new(0, 640))
                .is_err()
        );
        assert!(
            SessionHostConfig::new(output, OutputScale::default(), 60, Extent::new(8193, 640))
                .is_err()
        );
    }

    #[test]
    fn headless_callbacks_do_not_poll_at_refresh_while_idle_or_catch_up_in_bursts() {
        let start = Instant::now();
        let interval = Duration::from_millis(10);
        let mut clock = CallbackClock::new(interval);
        assert_eq!(clock.timeout(start, false), MAINTENANCE_INTERVAL);
        assert_eq!(clock.timeout(start, true), Duration::ZERO);
        clock.completed(start);
        assert_eq!(clock.timeout(start + interval / 2, true), interval / 2);
        let late = start + interval * 20;
        assert_eq!(clock.timeout(late, true), Duration::ZERO);
        clock.completed(late);
        assert_eq!(clock.timeout(late, true), interval);
        assert_eq!(clock.timeout(late, false), MAINTENANCE_INTERVAL);
    }
}
