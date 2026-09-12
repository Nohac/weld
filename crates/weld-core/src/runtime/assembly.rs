//! Assembly of ordinary client hosting without a local presenter.
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
use calloop::signals::{Signal, Signals};
use smithay::output::{PhysicalProperties, Subpixel};
use tracing::{info, warn};
use weld_client::{ClientAdapterRegistration, Extent};

use crate::{
    OutputId, OutputScale,
    dmabuf::{DmabufContext, DmabufSourceCache, ExternalDmabufCapabilities},
    host::{ApplicationHost, ClientRuntimeWakeSource, register_client_wake_sources},
    input::{KeyboardRepeatMode, LegacyKeyRepeat},
    runtime::{
        gpu::{NativeGpu, import_channel},
        native::{NativeRuntime, RuntimeIntegration, RuntimeSetup},
    },
    server::{
        OutputDescriptor, OutputMetrics, ServerOptions, ServerOutputDefinition,
        WaylandClientBridge, client_registration,
    },
};

/// Validated virtual output and startup window policy. Window sizes are logical
/// pixels; output extent is physical pixels. Neither is an encoder resolution.
/// The output describes a virtual display, not a bound on window geometry.
pub struct RuntimeOptions {
    output: OutputMetrics,
    initial_window_size: Extent,
    frame_interval: Duration,
    socket_name: Option<String>,
    client: Vec<OsString>,
    keyboard_repeat: KeyboardRepeatMode,
    legacy_repeat: LegacyKeyRepeat,
}

impl RuntimeOptions {
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
            "virtual refresh must be between 1 and 240 Hz"
        );
        ensure!(
            output.width <= 8192 && output.height <= 8192,
            "virtual output exceeds 8192 pixels per axis"
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

    /// Launches one command on this session's socket when [`HostRuntime::run`]
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
pub struct HostRuntime {
    runtime: NativeRuntime<()>,
    context: DmabufContext,
    config: RuntimeOptions,
    _gpu: Option<ImportGpu>,
    policy: Option<Box<dyn ApplicationHost>>,
}

impl HostRuntime {
    /// Opens the virtual Wayland host and optional GPU imports on the main thread.
    pub fn prepare(config: RuntimeOptions) -> Result<Self> {
        let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM])
            .context("failed to initialize process signal handling")?;
        let started_at = Instant::now();
        let gpu = ImportGpu::open().map(Some).unwrap_or_else(|error| {
            warn!(%error, "virtual GPU import unavailable; serving SHM clients only");
            None
        });
        let sources = gpu
            .as_ref()
            .map_or_else(DmabufSourceCache::unavailable, |gpu| {
                gpu.resources.sources.clone()
            });
        let capabilities = gpu
            .as_ref()
            .and_then(|gpu| gpu.resources.capabilities.clone());
        let (context, release_source) = import_channel(sources.clone(), capabilities.clone());
        let bridge = WaylandClientBridge::default();
        let registration = client_registration(bridge.clone(), context.clone())
            .into_parts()
            .runtime;
        let mut runtime = NativeRuntime::prepare(RuntimeSetup {
            server: ServerOptions {
                started_at,
                seat_name: "weld-seat0",
                outputs: vec![ServerOutputDefinition {
                    id: OutputId::new(1),
                    descriptor: OutputDescriptor {
                        name: "weld-virtual".to_owned(),
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
            releases: release_source,
            bridge,
            adapters: vec![registration],
            wakes: Vec::new(),
            signals,
            shutdown_event: || (),
        })?;
        runtime
            .state
            .data
            .server
            .set_legacy_key_repeat(config.legacy_repeat);
        Ok(Self {
            runtime,
            context,
            config,
            _gpu: gpu,
            policy: None,
        })
    }

    /// Adds a source/relay adapter; no image importer is needed without a local
    /// presenter. Runtime registration still enforces source namespace uniqueness.
    pub fn add_client_adapter(&mut self, adapter: ClientAdapterRegistration) -> Result<&mut Self> {
        self.runtime
            .state
            .clients
            .register(adapter.into_parts().runtime)?;
        Ok(self)
    }

    /// Registers adapter readiness with the same native event loop as Wayland.
    pub fn add_client_wake_source(&mut self, source: ClientRuntimeWakeSource) -> Result<&mut Self> {
        register_client_wake_sources(&self.runtime.event_loop.handle(), vec![source])?;
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

    /// Installs application policy without requiring a local compositor renderer.
    /// The same owner may expose composition, but no output is rendered here.
    pub fn with_policy(mut self, policy: impl ApplicationHost + 'static) -> Self {
        self.policy = Some(Box::new(policy));
        self
    }

    /// Serves clients until SIGINT/SIGTERM, even with no connected applications.
    /// Closing the display disconnects clients; this is not a process supervisor
    /// and does not kill arbitrary descendants of the launched command.
    pub fn run(mut self) -> Result<()> {
        self.runtime
            .state
            .children
            .spawn_requested(&self.runtime.state.data.server, &self.config.client)?;
        info!(socket = ?self.runtime.state.data.server.socket_name, width = self.config.output.physical_width(), height = self.config.output.physical_height(), scale = self.config.output.scale_factor(), "Weld client host is ready");
        self.runtime.run(
            RuntimeIntegration::Unpresented {
                application: self.policy,
            },
            self.config.frame_interval,
        )
    }
}

struct ImportGpu {
    _instance: wgpu::Instance,
    _adapter: wgpu::Adapter,
    resources: NativeGpu,
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
        .context("no Vulkan adapter for virtual client imports")?;
        let resources = NativeGpu::request(&adapter, "Weld client imports")?;
        Ok(Self {
            _instance: instance,
            _adapter: adapter,
            resources,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::MAINTENANCE_INTERVAL;
    use crate::runtime::native::CallbackClock;

    #[test]
    fn virtual_output_and_window_coordinates_are_independent() {
        let config = RuntimeOptions::new(
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
        let larger_window = RuntimeOptions::new(
            Extent::new(640, 480),
            OutputScale::default(),
            60,
            Extent::new(960, 640),
        )
        .expect("window is not clamped to the virtual display");
        assert_eq!(larger_window.initial_window_size, Extent::new(960, 640));
    }

    #[test]
    fn virtual_refresh_and_initial_policy_are_bounded() {
        let output = Extent::new(1920, 1080);
        for refresh in [0, 241, u32::MAX] {
            assert!(RuntimeOptions::new(output, OutputScale::default(), refresh, output).is_err());
        }
        assert!(
            RuntimeOptions::new(output, OutputScale::default(), 60, Extent::new(0, 640)).is_err()
        );
        assert!(
            RuntimeOptions::new(output, OutputScale::default(), 60, Extent::new(8193, 640))
                .is_err()
        );
    }

    #[test]
    fn virtual_callbacks_do_not_poll_at_refresh_while_idle_or_catch_up_in_bursts() {
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
