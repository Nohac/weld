//! Native compositor host, protocol server, GPU ownership, and backends.

pub(crate) const PROFILE_TARGET: &str = "weld_profile";

mod backend;
pub mod cursor;
pub mod dmabuf;
pub mod host;
pub mod input;
pub mod renderer;
pub mod runtime;
pub mod server;
pub mod surface;

pub use host::{
    CompositionDemand, CompositionHost, HostBackend, HostBuilder, OutputScale, PreparedHost,
    PreparedRuntime, RenderContext,
};
