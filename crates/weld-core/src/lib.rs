//! Native compositor host, protocol server, GPU ownership, and backends.

pub(crate) const PROFILE_TARGET: &str = "weld_profile";

/// Stable source namespace of the built-in Smithay Wayland adapter.
pub const WAYLAND_CLIENT_SOURCE: weld_client::ClientSourceId = weld_client::ClientSourceId::new(0);

mod backend;
pub mod cursor;
pub mod dmabuf;
pub mod geometry;
pub mod host;
pub mod input;
pub mod output;
pub mod renderer;
pub mod runtime;
pub mod server;
pub mod surface;

pub use host::{
    ApplicationHost, CompositionDemand, CompositionHost, HostBackend, HostBuilder, HostPolicy,
    PreparedHost, PreparedRuntime, RenderContext,
};
pub use output::{
    OutputConfiguration, OutputFootprint, OutputFootprintProvenance, OutputHead, OutputId,
    OutputLayout, OutputPhysicalSize, OutputScale, OutputTopology,
};
