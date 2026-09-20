//! Native media assembly, shared by initially connected and pending sources.

use std::path::PathBuf;

use anyhow::Result;
use weld_client::{ClientAdapterRegistration, ClientSourceId};
use weld_core::{
    dmabuf::{DmabufContext, ExternalDmabufCapabilities},
    host::{ClientRuntimeWakeSource, client_runtime_notifier},
};
use weld_hoist_core::gamepad::GamepadProvider;
use weld_hoist_encoded::{
    EncodeBackend, SharedBitrateBudget, decode_backend, encode_backend,
    native::DecodedDmabufPublisher,
};
use weld_media::VideoCodec;

use crate::{
    IrohDestinationEndpoint, IrohDestinationPeer, IrohSourceOptions, IrohSourcePeer,
    PendingSourceAdmission, destination_registration_with_backend,
    pending_source_registration_with_backend, source_registration_with_backend,
};

/// Source media resources; remote namespace mapping belongs to the manual endpoint.
pub struct IrohSourceRegistrationOptions<'a> {
    pub upstream_source: ClientSourceId,
    pub adapter_source: ClientSourceId,
    pub capabilities: &'a ExternalDmabufCapabilities,
    pub codec: VideoCodec,
    pub dump_directory: Option<PathBuf>,
    pub bitrate_budget: Option<SharedBitrateBudget>,
    pub gamepad: Option<Box<dyn GamepadProvider>>,
}

pub fn source_registration(
    peer: IrohSourcePeer,
    destination_source: ClientSourceId,
    options: IrohSourceRegistrationOptions<'_>,
) -> Result<(
    ClientAdapterRegistration,
    IrohDestinationEndpoint,
    ClientRuntimeWakeSource,
)> {
    let (backend, wake) = source_backend(&options)?;
    let (registration, endpoint) = source_registration_with_backend(
        peer,
        options.upstream_source,
        options.adapter_source,
        destination_source,
        backend,
        IrohSourceOptions {
            dump_directory: options
                .dump_directory
                .map(|directory| (directory, options.codec)),
            bitrate_budget: options.bitrate_budget,
            gamepad: options.gamepad,
        },
    )?;
    Ok((registration, endpoint, wake))
}

fn source_backend(
    options: &IrohSourceRegistrationOptions<'_>,
) -> Result<(Box<dyn EncodeBackend>, ClientRuntimeWakeSource)> {
    let (notifier, wake) = client_runtime_notifier()?;
    let backend = encode_backend(
        options.capabilities.render_node.clone(),
        options.codec,
        options.dump_directory.clone(),
        move || {
            if let Err(error) = notifier.notify() {
                tracing::error!(%error, "could not wake Weld for encoded Iroh output");
            }
        },
    )?;
    Ok((backend, wake))
}

pub fn destination_registration(
    peer: IrohDestinationPeer,
    upstream_source: ClientSourceId,
    destination_source: ClientSourceId,
    dmabuf: DmabufContext,
    capabilities: &ExternalDmabufCapabilities,
    codec: VideoCodec,
) -> Result<(ClientAdapterRegistration, ClientRuntimeWakeSource)> {
    let (notifier, wake) = client_runtime_notifier()?;
    let backend = decode_backend(capabilities, codec, move || {
        if let Err(error) = notifier.notify() {
            tracing::error!(%error, "could not wake Weld for decoded Iroh output");
        }
    })?;
    Ok((
        destination_registration_with_backend(
            peer,
            upstream_source,
            destination_source,
            DecodedDmabufPublisher::new(dmabuf),
            backend,
            None,
        ),
        wake,
    ))
}

/// Installs a pending source using the ordinary VA-API backend and wake integration.
pub fn pending_source_registration(
    pending: PendingSourceAdmission,
    options: IrohSourceRegistrationOptions<'_>,
) -> Result<(ClientAdapterRegistration, ClientRuntimeWakeSource)> {
    let (backend, wake) = source_backend(&options)?;
    Ok((
        pending_source_registration_with_backend(
            pending,
            options.upstream_source,
            options.adapter_source,
            backend,
            IrohSourceOptions {
                dump_directory: options.dump_directory.map(|path| (path, options.codec)),
                bitrate_budget: options.bitrate_budget,
                gamepad: options.gamepad,
            },
        ),
        wake,
    ))
}
