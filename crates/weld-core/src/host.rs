//! Application-host contract driven by Weld's native backends.

use std::{ffi::OsString, path::PathBuf};

use anyhow::{Context, Result};
use calloop::signals::{Signal, Signals};
use tracing::warn;

use crate::{
    dmabuf::DmabufContext,
    input::{RawSeatEvent, SeatInputEffect},
    output::{OutputConfiguration, OutputHead, OutputId, OutputScale},
    runtime::HostCommand,
    server::PendingSurfaceEvent,
    surface::{Extent, SurfaceAction},
};

/// Distribution options consumed by either host backend.
#[derive(Default)]
pub(crate) struct RunOptions {
    pub(crate) client: Vec<OsString>,
    pub(crate) screenshot: Option<PathBuf>,
    pub(crate) remote_debug_enabled: bool,
    pub(crate) output_scale: OutputScale,
}

/// Native host selected before an application is constructed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HostBackend {
    #[default]
    Nested,
    Drm,
}

/// Configures and opens a native compositor host before application setup.
#[derive(Default)]
pub struct HostBuilder {
    backend: HostBackend,
    options: RunOptions,
    output_scale: Option<OutputScale>,
}

impl HostBuilder {
    /// Creates a builder that prepares the nested backend by default.
    pub fn new() -> Self {
        Self::default()
    }

    /// Selects the native backend to prepare.
    pub fn backend(mut self, backend: HostBackend) -> Self {
        self.backend = backend;
        self
    }

    /// Configures an optional client command to launch once the host is ready.
    pub fn launch<I, S>(mut self, command: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.options.client = command.into_iter().map(Into::into).collect();
        self
    }

    /// Requests a startup screenshot at the given path.
    pub fn screenshot(mut self, path: Option<PathBuf>) -> Self {
        self.options.screenshot = path;
        self
    }

    /// Records whether the host must remain live for remote capture requests.
    pub fn remote_debug_enabled(mut self, enabled: bool) -> Self {
        self.options.remote_debug_enabled = enabled;
        self
    }

    /// Configures an explicit scale for standalone DRM output.
    pub fn output_scale(mut self, scale: Option<OutputScale>) -> Self {
        self.output_scale = scale;
        self
    }

    /// Opens the selected backend and GPU resources on the current thread.
    pub fn prepare(mut self) -> Result<PreparedHost> {
        // Install the signalfd mask before the backend creates wgpu or any
        // application workers. Subsequently created threads inherit it.
        let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM])
            .context("failed to initialize process signal handling")?;
        if self.output_scale.is_some() && self.backend == HostBackend::Nested {
            warn!("ignored explicit output scale because the nested backend follows its host");
        }
        self.options.output_scale = self.output_scale.unwrap_or_default();
        match self.backend {
            HostBackend::Nested => crate::backend::nested::prepare(self.options, signals),
            HostBackend::Drm => crate::backend::drm::prepare(self.options, signals),
        }
    }
}

#[cfg(test)]
mod output_scale_tests {
    use super::OutputScale;
    use crate::runtime::OutputScaleAdjustment;

    #[test]
    fn output_scale_rejects_non_positive_and_non_finite_values() {
        assert_eq!(OutputScale::new(1.25).expect("valid scale").value(), 1.25);
        assert!(OutputScale::new(0.0).is_err());
        assert!(OutputScale::new(-1.0).is_err());
        assert!(OutputScale::new(f64::NAN).is_err());
        assert!(OutputScale::new(f64::INFINITY).is_err());
        assert!("0".parse::<OutputScale>().is_err());
    }

    #[test]
    fn output_scale_adjustments_snap_to_directional_quarter_steps() {
        let scale = OutputScale::new(1.1).expect("valid scale");
        assert_eq!(
            scale.adjust(OutputScaleAdjustment::Increase),
            Some(OutputScale::new(1.25).expect("valid scale"))
        );
        assert_eq!(
            scale.adjust(OutputScaleAdjustment::Decrease),
            Some(OutputScale::new(1.0).expect("valid scale"))
        );
        assert_eq!(
            OutputScale::new(0.25)
                .expect("valid scale")
                .adjust(OutputScaleAdjustment::Decrease),
            None
        );
        assert_eq!(
            OutputScale::new(0.1)
                .expect("valid scale")
                .adjust(OutputScaleAdjustment::Decrease),
            None
        );
    }
}

/// GPU and output state required to construct a Bevy-backed application host.
#[derive(Clone)]
pub struct RenderContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub dmabuf: DmabufContext,
    pub output_heads: Vec<OutputHead>,
    pub outputs: Vec<OutputConfiguration>,
    pub composition_format: wgpu::TextureFormat,
}

type RunPreparedHost = Box<dyn FnOnce(Box<dyn CompositionHost>) -> Result<()>>;

/// Native event-loop state ready to drive one application host.
pub struct PreparedRuntime {
    run: RunPreparedHost,
}

impl PreparedRuntime {
    pub(crate) fn new(run: impl FnOnce(Box<dyn CompositionHost>) -> Result<()> + 'static) -> Self {
        Self { run: Box::new(run) }
    }

    /// Drives the prepared native event loop with one application host.
    ///
    /// This must run on the thread that prepared the host.
    pub fn run(self, host: impl CompositionHost + 'static) -> Result<()> {
        (self.run)(Box::new(host))
    }
}

/// A native host whose GPU is ready for application construction.
pub struct PreparedHost {
    context: RenderContext,
    runtime: PreparedRuntime,
}

impl PreparedHost {
    pub(crate) fn new(
        context: RenderContext,
        run: impl FnOnce(Box<dyn CompositionHost>) -> Result<()> + 'static,
    ) -> Self {
        Self {
            context,
            runtime: PreparedRuntime::new(run),
        }
    }

    /// Borrows the GPU context needed to construct an application host.
    pub const fn render_context(&self) -> &RenderContext {
        &self.context
    }

    /// Separates the GPU context from the one-shot native runtime.
    pub fn into_parts(self) -> (RenderContext, PreparedRuntime) {
        (self.context, self.runtime)
    }
}

#[derive(Debug)]
pub struct CaptureRequest {
    pub request_id: u64,
    pub path: PathBuf,
}

/// Amount of Bevy composition work requested by one host surface event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompositionDemand {
    /// One composition is sufficient for an ordinary content update.
    Ordinary,
    /// Several paced compositions are required for deferred Bevy work to converge.
    Settle,
}

/// Bevy-independent interface through which a backend drives application policy and composition.
pub trait CompositionHost {
    fn enqueue_surface_event(&mut self, event: PendingSurfaceEvent) -> CompositionDemand;
    /// Buffers an input event for the next application frame and returns
    /// whether core should also forward it to the focused client immediately.
    fn enqueue_input_event(&mut self, event: RawSeatEvent) -> bool;
    fn advance_main(&mut self, input_time: u32) -> bool;
    /// Services the restricted remote-control schedule without advancing the
    /// application world.
    fn service_remote_debug(&mut self);
    /// Fills `frames` with the completed outputs.
    ///
    /// The buffer is cleared before rendering, retains its allocation between
    /// calls, and contains a complete composition only when this returns `Ok`.
    fn render_outputs(
        &mut self,
        requests: &[CompositionOutputRequest],
        frames: &mut Vec<CompositionOutputFrame>,
    ) -> Result<()>;
    /// Reconciles enabled output geometry before the next main advance.
    fn update_output_topology(&mut self, outputs: &[OutputConfiguration]);
    fn should_exit(&self) -> bool;
    fn take_input_effects(&mut self) -> Vec<SeatInputEffect>;
    fn take_cursor_update(&mut self) -> crate::cursor::CursorHostUpdate;
    fn take_host_commands(&mut self) -> Vec<HostCommand>;
    fn take_virtual_terminal_switch_request(&mut self) -> Option<i32>;
    fn take_surface_actions(&mut self) -> Vec<SurfaceAction>;
    fn has_surface_frame(&self) -> bool;
    fn take_capture_request(&mut self) -> Option<CaptureRequest>;
    fn complete_capture(&mut self, request_id: u64, result: Result<(), String>);
}

/// One concrete GPU view selected for the next application composition.
#[derive(Clone)]
pub struct CompositionTargetView {
    view: wgpu::TextureView,
    extent: Extent,
    format: wgpu::TextureFormat,
}

impl CompositionTargetView {
    pub fn new(view: wgpu::TextureView, extent: Extent, format: wgpu::TextureFormat) -> Self {
        Self {
            view,
            extent,
            format,
        }
    }

    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub const fn extent(&self) -> Extent {
        self.extent
    }

    pub const fn format(&self) -> wgpu::TextureFormat {
        self.format
    }
}

/// Selects whether the application renders into its retained target or a
/// backend-leased external target for this composition.
#[derive(Clone)]
pub enum CompositionDestination {
    Owned,
    External(CompositionTargetView),
}

/// Output and destination selected for one member of an atomic composition.
pub struct CompositionOutputRequest {
    pub output: OutputId,
    pub destination: CompositionDestination,
}

/// Output identity paired with its completed composition.
pub struct CompositionOutputFrame {
    pub output: OutputId,
    pub frame: CompositionFrame,
}

/// Completed application composition and any storage retained for readback.
pub struct CompositionFrame {
    target: CompositionTargetView,
    owned_texture: Option<wgpu::Texture>,
}

impl CompositionFrame {
    pub fn owned(target: CompositionTargetView, texture: wgpu::Texture) -> Self {
        Self {
            target,
            owned_texture: Some(texture),
        }
    }

    pub fn external(target: CompositionTargetView) -> Self {
        Self {
            target,
            owned_texture: None,
        }
    }

    pub const fn target(&self) -> &CompositionTargetView {
        &self.target
    }

    pub fn owned_texture(&self) -> Option<&wgpu::Texture> {
        self.owned_texture.as_ref()
    }
}
