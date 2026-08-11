//! Application-host contract driven by Weld's native backends.

use std::{ffi::OsString, path::PathBuf};

use anyhow::Result;

use crate::{
    dmabuf::DmabufContext,
    input::{InputPosition, RawSeatEvent, SeatInputEffect},
    runtime::HostCommand,
    server::PendingSurfaceEvent,
    surface::{Extent, SurfaceAction},
};

/// Distribution options consumed by either host backend.
pub struct RunOptions {
    pub client: Vec<OsString>,
    pub screenshot: Option<PathBuf>,
    pub remote_debug_enabled: bool,
}

/// GPU and output state required to construct a Bevy-backed application host.
#[derive(Clone)]
pub struct RenderContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub dmabuf: DmabufContext,
    pub extent: Extent,
    pub scale_factor: f64,
    pub initial_target: wgpu::TextureView,
}

#[derive(Debug)]
pub struct CaptureRequest {
    pub request_id: u64,
    pub path: PathBuf,
}

/// Bevy-independent interface through which a backend drives application policy and composition.
pub trait CompositionHost {
    fn enqueue_surface_event(&mut self, event: PendingSurfaceEvent);
    fn enqueue_input_event(&mut self, event: RawSeatEvent);
    fn advance_main(&mut self, input_time: u32, composition_advance: bool) -> bool;
    fn render_composition(&mut self, target: wgpu::TextureView, extent: Extent) -> Result<()>;
    /// Register output geometry and a composition target before the next main advance.
    ///
    /// The target must be free for the application to render into. Implementations
    /// must make the view visible to their renderer before the next
    /// [`Self::advance_main`] returns so layout observes the matching extent.
    fn set_output_geometry(&mut self, target: wgpu::TextureView, extent: Extent, scale_factor: f64);
    fn should_exit(&self) -> bool;
    fn pointer_position(&self) -> Option<InputPosition>;
    fn take_input_effects(&mut self) -> Vec<SeatInputEffect>;
    fn take_host_commands(&mut self) -> Vec<HostCommand>;
    fn take_virtual_terminal_switch_request(&mut self) -> Option<i32>;
    fn take_surface_actions(&mut self) -> Vec<SurfaceAction>;
    fn has_surface_frame(&self) -> bool;
    fn take_capture_request(&mut self) -> Option<CaptureRequest>;
    fn complete_capture(&mut self, request_id: u64, result: Result<(), String>);
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CompositionTargetId(usize);

impl CompositionTargetId {
    pub(crate) const FIRST: Self = Self(0);
    pub(crate) const SECOND: Self = Self(1);
}

struct CompositionTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

pub(crate) struct CompositionTargets {
    targets: [CompositionTarget; 2],
    completed: CompositionTargetId,
    extent: Extent,
}

impl CompositionTargets {
    pub(crate) fn new(device: &wgpu::Device, extent: Extent) -> Self {
        Self {
            targets: [create_target(device, extent), create_target(device, extent)],
            completed: CompositionTargetId::FIRST,
            extent,
        }
    }

    pub(crate) fn resize(&mut self, device: &wgpu::Device, extent: Extent) {
        self.targets = [create_target(device, extent), create_target(device, extent)];
        self.completed = CompositionTargetId::FIRST;
        self.extent = extent;
    }

    pub(crate) const fn ids(&self) -> [CompositionTargetId; 2] {
        [CompositionTargetId::FIRST, CompositionTargetId::SECOND]
    }

    pub(crate) const fn completed(&self) -> CompositionTargetId {
        self.completed
    }

    pub(crate) fn mark_completed(&mut self, target: CompositionTargetId) {
        self.completed = target;
    }

    pub(crate) fn view(&self, target: CompositionTargetId) -> &wgpu::TextureView {
        &self.targets[target.0].view
    }

    pub(crate) fn texture(&self, target: CompositionTargetId) -> &wgpu::Texture {
        &self.targets[target.0].texture
    }

    pub(crate) const fn extent(&self) -> Extent {
        self.extent
    }
}

fn create_target(device: &wgpu::Device, extent: Extent) -> CompositionTarget {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("weld Bevy composition target"),
        size: wgpu::Extent3d {
            width: extent.width,
            height: extent.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    CompositionTarget { texture, view }
}
