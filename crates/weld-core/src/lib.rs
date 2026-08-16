//! Native compositor host, protocol server, GPU ownership, and backends.

pub(crate) const PROFILE_TARGET: &str = "weld_profile";

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
    CompositionDemand, CompositionHost, HostBackend, HostBuilder, PreparedHost, PreparedRuntime,
    RenderContext,
};
pub use output::{
    OutputConfiguration, OutputFootprint, OutputFootprintProvenance, OutputHead, OutputId,
    OutputLayout, OutputPhysicalSize, OutputScale, OutputTopology,
};
