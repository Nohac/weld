//! Native media assembly, shared by initially connected and pending sources.

use std::path::PathBuf;

use crate::{
    EncodeBackend, SharedBitrateBudget, decode_backend, encode_backend,
    native::DecodedDmabufPublisher,
};
use anyhow::Result;
use weld_client::{ClientAdapterRegistration, ClientSourceId};
use weld_core::{
    dmabuf::{DmabufContext, ExternalDmabufCapabilities},
    host::{ClientRuntimeWakeSource, client_runtime_notifier},
};
use weld_hoist_core::gamepad::GamepadProvider;
use weld_media::VideoCodec;

use crate::{
    DestinationEndpoint, EncodedDestinationTransport, PendingSource, SourceConnection,
    SourceOptions, destination_registration_with_backend, pending_source_registration_with_backend,
    source_registration_with_backend,
};

/// Source media resources; remote namespace mapping belongs to the manual endpoint.
pub struct SourceRegistrationOptions<'a> {
    pub upstream_source: ClientSourceId,
    pub adapter_source: ClientSourceId,
    pub capabilities: &'a ExternalDmabufCapabilities,
    pub codec: VideoCodec,
    pub dump_directory: Option<PathBuf>,
    pub bitrate_budget: Option<SharedBitrateBudget>,
    pub gamepad: Option<Box<dyn GamepadProvider>>,
}

pub fn source_registration<T: SourceConnection>(
    peer: T,
    destination_source: ClientSourceId,
    options: SourceRegistrationOptions<'_>,
) -> Result<(
    ClientAdapterRegistration,
    DestinationEndpoint<T>,
    ClientRuntimeWakeSource,
)> {
    let (backend, wake) = source_backend(&options)?;
    let (registration, endpoint) = source_registration_with_backend(
        peer,
        options.upstream_source,
        options.adapter_source,
        destination_source,
        backend,
        SourceOptions {
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
    options: &SourceRegistrationOptions<'_>,
) -> Result<(Box<dyn EncodeBackend>, ClientRuntimeWakeSource)> {
    let (notifier, wake) = client_runtime_notifier()?;
    let backend = encode_backend(
        options.capabilities.render_node.clone(),
        options.codec,
        options.dump_directory.clone(),
        move || {
            if let Err(error) = notifier.notify() {
                tracing::error!(%error, "could not wake Weld for encoded output");
            }
        },
    )?;
    Ok((backend, wake))
}

pub fn destination_registration<T: EncodedDestinationTransport + 'static>(
    peer: T,
    upstream_source: ClientSourceId,
    destination_source: ClientSourceId,
    dmabuf: DmabufContext,
    capabilities: &ExternalDmabufCapabilities,
    codec: VideoCodec,
) -> Result<(ClientAdapterRegistration, ClientRuntimeWakeSource)> {
    let (notifier, wake) = client_runtime_notifier()?;
    let backend = decode_backend(capabilities, codec, move || {
        if let Err(error) = notifier.notify() {
            tracing::error!(%error, "could not wake Weld for decoded output");
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
pub fn pending_source_registration<P: PendingSource>(
    pending: P,
    options: SourceRegistrationOptions<'_>,
) -> Result<(ClientAdapterRegistration, ClientRuntimeWakeSource)> {
    let (backend, wake) = source_backend(&options)?;
    Ok((
        pending_source_registration_with_backend(
            pending,
            options.upstream_source,
            options.adapter_source,
            backend,
            SourceOptions {
                dump_directory: options.dump_directory.map(|path| (path, options.codec)),
                bitrate_budget: options.bitrate_budget,
                gamepad: options.gamepad,
            },
        ),
        wake,
    ))
}
