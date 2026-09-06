//! Client-adapter registrations over one encoded Iroh peer.

use std::path::PathBuf;

use weld_client::{
    ClientAdapterCommandEnvelope, ClientAdapterRegistration, ClientProvenance,
    ClientSourceDescriptor, ClientSourceId, ClientSurfaceId, ControlOnlyClientImporter,
};
use weld_core::dmabuf::{DirectClientBufferImporter, DmabufContext};
use weld_hoist_core::{
    DestinationRelayAdapter, HoistEndpoint, HoistEndpointCommand, HoistSessionId,
    SourceRelayAdapter, relocated_surface,
};
use weld_hoist_encoded::{
    DecodeBackend, EncodeBackend, EncodedDestinationPort, EncodedSourcePort, EncoderRateControl,
};
use weld_media::VideoCodec;

use crate::{IrohDestinationPeer, IrohSourcePeer};

#[cfg(feature = "vaapi")]
use weld_core::dmabuf::ExternalDmabufCapabilities;
#[cfg(feature = "vaapi")]
use weld_hoist_encoded::{decode_backend, encode_backend};

/// Source policy endpoint associated with one remote Iroh destination.
#[derive(Clone)]
pub struct IrohDestinationEndpoint {
    adapter_source: ClientSourceId,
    destination_source: ClientSourceId,
    peer: IrohSourcePeer,
    rate_control: Option<EncoderRateControl>,
}

impl IrohDestinationEndpoint {
    /// Optional source-side actuator; the handle expires with its registered adapter.
    pub fn encoder_rate_control(&self) -> Option<EncoderRateControl> {
        self.rate_control.clone()
    }
}

impl HoistEndpoint for IrohDestinationEndpoint {
    fn is_available(&self) -> bool {
        self.peer.is_available()
    }

    fn has_local_receiver(&self) -> bool {
        false
    }

    fn destination(&self, source: ClientSurfaceId) -> ClientSurfaceId {
        relocated_surface(self.destination_source, source)
    }

    fn map(
        &self,
        session: HoistSessionId,
        source: ClientSurfaceId,
    ) -> ClientAdapterCommandEnvelope {
        ClientAdapterCommandEnvelope::new(
            self.adapter_source,
            HoistEndpointCommand::Map { session, source },
        )
    }

    fn unmap(&self, source: ClientSurfaceId) -> ClientAdapterCommandEnvelope {
        ClientAdapterCommandEnvelope::new(
            self.adapter_source,
            HoistEndpointCommand::Unmap { source },
        )
    }
}

#[cfg(feature = "vaapi")]
pub struct IrohSourceRegistrationOptions<'a> {
    pub upstream_source: ClientSourceId,
    pub adapter_source: ClientSourceId,
    pub destination_source: ClientSourceId,
    pub capabilities: &'a ExternalDmabufCapabilities,
    pub codec: VideoCodec,
    pub dump_directory: Option<PathBuf>,
}

pub fn source_registration_with_backend(
    peer: IrohSourcePeer,
    upstream_source: ClientSourceId,
    adapter_source: ClientSourceId,
    destination_source: ClientSourceId,
    backend: Box<dyn EncodeBackend>,
    dump_directory: Option<(PathBuf, VideoCodec)>,
) -> anyhow::Result<(ClientAdapterRegistration, IrohDestinationEndpoint)> {
    let descriptor = ClientSourceDescriptor::new(adapter_source, ClientProvenance::Relocated);
    let mut port = EncodedSourcePort::new(peer.clone(), backend);
    if let Some((directory, codec)) = dump_directory {
        port = port.with_access_unit_dump_directory(directory, codec)?;
    }
    let rate_control = port.encoder_rate_control();
    let adapter = SourceRelayAdapter::new(upstream_source, port);
    Ok((
        ClientAdapterRegistration::new(descriptor, adapter, ControlOnlyClientImporter),
        IrohDestinationEndpoint {
            adapter_source,
            destination_source,
            peer,
            rate_control,
        },
    ))
}

pub fn destination_registration_with_backend(
    peer: IrohDestinationPeer,
    upstream_source: ClientSourceId,
    destination_source: ClientSourceId,
    dmabuf: DmabufContext,
    backend: Box<dyn DecodeBackend>,
) -> ClientAdapterRegistration {
    let descriptor = ClientSourceDescriptor::new(destination_source, ClientProvenance::Relocated);
    ClientAdapterRegistration::new(
        descriptor,
        DestinationRelayAdapter::new(
            upstream_source,
            descriptor,
            EncodedDestinationPort::new(peer, backend, descriptor, dmabuf),
        ),
        DirectClientBufferImporter,
    )
}

#[cfg(feature = "vaapi")]
pub fn source_registration(
    peer: IrohSourcePeer,
    options: IrohSourceRegistrationOptions<'_>,
) -> anyhow::Result<(
    ClientAdapterRegistration,
    IrohDestinationEndpoint,
    weld_core::host::ClientRuntimeWakeSource,
)> {
    let (notifier, wake) = weld_core::host::client_runtime_notifier()?;
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
    let (registration, endpoint) = source_registration_with_backend(
        peer,
        options.upstream_source,
        options.adapter_source,
        options.destination_source,
        backend,
        options
            .dump_directory
            .map(|directory| (directory, options.codec)),
    )?;
    Ok((registration, endpoint, wake))
}

#[cfg(feature = "vaapi")]
pub fn destination_registration(
    peer: IrohDestinationPeer,
    upstream_source: ClientSourceId,
    destination_source: ClientSourceId,
    dmabuf: DmabufContext,
    capabilities: &ExternalDmabufCapabilities,
    codec: VideoCodec,
) -> anyhow::Result<(
    ClientAdapterRegistration,
    weld_core::host::ClientRuntimeWakeSource,
)> {
    let (notifier, wake) = weld_core::host::client_runtime_notifier()?;
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
            dmabuf,
            backend,
        ),
        wake,
    ))
}
