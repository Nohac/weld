//! Distribution assembly: ordinary client hosting plus optional whole-session
//! Iroh admission. Neither core nor the transport needs a headless mode flag.

use anyhow::{Context, Result};
use weld_client::ClientSourceId;
use weld_core::{host::client_runtime_notifier, runtime::HostRuntime};
use weld_hoist_iroh::{IrohHost, IrohSourceRegistrationOptions, pending_source_registration};

use crate::{
    AppArguments, MediaOperation, bitrate_budget, runtime_options, validate_encoded_capabilities,
};

pub(crate) fn run(arguments: AppArguments) -> Result<()> {
    let budget = bitrate_budget::for_source(&arguments)?;
    // Prepare first so GPU/network workers inherit the host shutdown mask.
    let mut runtime = HostRuntime::prepare(runtime_options(&arguments)?)?;
    let Some(ticket) = &arguments.hoist_iroh_listen else {
        return runtime.run();
    };
    let host = IrohHost::bind(arguments.hoist_iroh_network.unwrap_or_default().into())?;
    let codec = arguments.hoist_codec.unwrap_or_default().into();
    let capabilities = runtime
        .external_dmabuf_capabilities()?
        .context("selected GPU cannot import DMA-BUFs")?;
    validate_encoded_capabilities(Some(&capabilities), codec, MediaOperation::Encode)?;
    let (notifier, network_wake) = client_runtime_notifier()?;
    let pending = host.begin_accept_source(
        ticket,
        arguments
            .hoist_iroh_expect_peer
            .as_ref()
            .context("Iroh source requires an approved peer identity")?,
        codec,
        notifier.into(),
        std::time::Duration::from_secs(arguments.hoist_iroh_timeout.unwrap_or(120)),
    )?;
    let (adapter, codec_wake) = pending_source_registration(
        pending,
        IrohSourceRegistrationOptions {
            upstream_source: weld_core::WAYLAND_CLIENT_SOURCE,
            adapter_source: ClientSourceId::new(1),
            capabilities: &capabilities,
            codec,
            dump_directory: arguments.hoist_encoded_dump_dir,
            bitrate_budget: budget,
        },
    )?;
    runtime
        .add_client_wake_source(network_wake)?
        .add_client_wake_source(codec_wake)?
        .add_client_adapter(adapter)?;
    runtime.run()
}
