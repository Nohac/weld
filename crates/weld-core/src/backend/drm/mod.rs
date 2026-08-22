//! Standalone DRM host boundary.
//!
//! The previous low-level presenter was removed before rebuilding this backend
//! around Smithay's output compositor. Keeping this boundary explicit prevents
//! automatic backend selection from silently falling back to a different host.

use anyhow::Result;
use calloop::signals::Signals;

use crate::host::{PreparedHost, RunOptions};

mod cursor;
mod device;
mod host;
mod output;
mod presentation;
mod renderer;
mod schedule;
mod vulkan;

pub(crate) fn prepare(options: RunOptions, signals: Signals) -> Result<PreparedHost> {
    let bootstrap = device::prepare(&options)?;
    let context = bootstrap.render_context;
    let runtime = bootstrap.runtime;
    Ok(PreparedHost::new(context, move |application| {
        host::run(runtime, options, signals, application)
    }))
}
