//! VA-API assembly of the portable pending source port.

use std::path::PathBuf;

use anyhow::Result;
use weld_client::{ClientAdapterRegistration, ClientSourceId};
use weld_core::{
    dmabuf::ExternalDmabufCapabilities,
    host::{ClientRuntimeWakeSource, client_runtime_notifier},
};
use weld_hoist_encoded::{SharedBitrateBudget, encode_backend};
use weld_media::VideoCodec;

use crate::{PendingSourceAdmission, pending_source_registration_with_backend};

/// Native resources for automatic source admission, without a manual policy endpoint.
pub struct PendingSourceRegistrationOptions<'a> {
    pub upstream_source: ClientSourceId,
    pub adapter_source: ClientSourceId,
    pub capabilities: &'a ExternalDmabufCapabilities,
    pub codec: VideoCodec,
    pub dump_directory: Option<PathBuf>,
    pub bitrate_budget: Option<SharedBitrateBudget>,
}

/// Installs a pending source using the ordinary VA-API backend and wake integration.
pub fn pending_source_registration(
    pending: PendingSourceAdmission,
    options: PendingSourceRegistrationOptions<'_>,
) -> Result<(ClientAdapterRegistration, ClientRuntimeWakeSource)> {
    let (notifier, wake) = client_runtime_notifier()?;
    let backend = encode_backend(
        options.capabilities.render_node.clone(),
        options.codec,
        options.dump_directory.clone(),
        move || {
            if let Err(error) = notifier.notify() {
                tracing::error!(%error, "could not wake pending Iroh source for encoder output");
            }
        },
    )?;
    Ok((
        pending_source_registration_with_backend(
            pending,
            options.upstream_source,
            options.adapter_source,
            backend,
            options.dump_directory.map(|path| (path, options.codec)),
            options.bitrate_budget,
        ),
        wake,
    ))
}
