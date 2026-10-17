#[cfg(feature = "encoded-vaapi")]
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
    DecodedFramePublisher, DestinationEndpoint, EncodedSourceOptions, SharedBitrateBudget,
    SourceOptions,
};

use crate::{
    LocalPacketConnection,
    destination::LocalDestinationPort,
    encoded_transport::{LocalEncodedDestinationTransport, LocalEncodedSourceTransport},
    source::LocalSourcePort,
};

#[cfg(feature = "encoded-vaapi")]
use weld_hoist_encoded::native::registration;

#[derive(Clone)]
pub struct LocalDestinationEndpoint {
    adapter_source: ClientSourceId,
    destination_source: ClientSourceId,
    connection: LocalPacketConnection,
}

impl HoistEndpoint for LocalDestinationEndpoint {
    fn is_available(&self) -> bool {
        !self.connection.is_disconnected()
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

pub fn local_source_registration(
    connection: LocalPacketConnection,
    upstream_source: ClientSourceId,
    adapter_source: ClientSourceId,
    destination_source: ClientSourceId,
) -> (ClientAdapterRegistration, LocalDestinationEndpoint) {
    let descriptor = ClientSourceDescriptor::new(adapter_source, ClientProvenance::Relocated);
    let adapter =
        SourceRelayAdapter::new(upstream_source, LocalSourcePort::new(connection.clone()));
    (
        ClientAdapterRegistration::new(descriptor, adapter, ControlOnlyClientImporter),
        LocalDestinationEndpoint {
            adapter_source,
            destination_source,
            connection,
        },
    )
}

pub fn local_destination_registration(
    connection: LocalPacketConnection,
    upstream_source: ClientSourceId,
    destination_source: ClientSourceId,
    dmabuf: DmabufContext,
) -> ClientAdapterRegistration {
    let descriptor = ClientSourceDescriptor::new(destination_source, ClientProvenance::Relocated);
    ClientAdapterRegistration::new(
        descriptor,
        DestinationRelayAdapter::new(
            upstream_source,
            descriptor,
            LocalDestinationPort::new(connection, descriptor, dmabuf),
        ),
        DirectClientBufferImporter,
    )
}

pub fn encoded_source_registration_with_backend(
    control: LocalPacketConnection,
    media: LocalPacketConnection,
    upstream_source: ClientSourceId,
    adapter_source: ClientSourceId,
    destination_source: ClientSourceId,
    backend: Box<dyn crate::LocalEncodeBackend>,
    bitrate_budget: Option<SharedBitrateBudget>,
) -> anyhow::Result<(
    ClientAdapterRegistration,
    DestinationEndpoint<LocalEncodedSourceTransport>,
)> {
    configured_source_registration(
        control,
        media,
        upstream_source,
        adapter_source,
        destination_source,
        backend,
        EncodedSourceOptions {
            bitrate_budget,
            access_unit_dump: None,
        },
    )
}

fn configured_source_registration(
    control: LocalPacketConnection,
    media: LocalPacketConnection,
    upstream_source: ClientSourceId,
    adapter_source: ClientSourceId,
    destination_source: ClientSourceId,
    backend: Box<dyn crate::LocalEncodeBackend>,
    options: EncodedSourceOptions,
) -> anyhow::Result<(
    ClientAdapterRegistration,
    DestinationEndpoint<LocalEncodedSourceTransport>,
)> {
    weld_hoist_encoded::source_registration_with_backend(
        LocalEncodedSourceTransport::new(control, media),
        upstream_source,
        adapter_source,
        destination_source,
        backend,
        SourceOptions {
            dump_directory: options.access_unit_dump,
            bitrate_budget: options.bitrate_budget,
            gamepad: None,
        },
    )
}

pub fn encoded_destination_registration_with_backend<P: DecodedFramePublisher>(
    control: LocalPacketConnection,
    media: LocalPacketConnection,
    upstream_source: ClientSourceId,
    destination_source: ClientSourceId,
    publisher: P,
    backend: Box<dyn crate::LocalDecodeBackend<Output = P::Buffer>>,
) -> anyhow::Result<ClientAdapterRegistration> {
    Ok(weld_hoist_encoded::destination_registration_with_backend(
        LocalEncodedDestinationTransport::new(control, media)?,
        upstream_source,
        destination_source,
        publisher,
        backend,
        None,
    ))
}

#[cfg(feature = "encoded-vaapi")]
pub struct EncodedSourceRegistrationOptions<'a> {
    pub upstream_source: ClientSourceId,
    pub adapter_source: ClientSourceId,
    pub destination_source: ClientSourceId,
    pub capabilities: &'a weld_core::dmabuf::ExternalDmabufCapabilities,
    pub codec: weld_media::VideoCodec,
    pub dump_directory: Option<PathBuf>,
    pub bitrate_budget: Option<SharedBitrateBudget>,
}

#[cfg(feature = "encoded-vaapi")]
pub fn encoded_source_registration(
    control: LocalPacketConnection,
    media: LocalPacketConnection,
    options: EncodedSourceRegistrationOptions<'_>,
) -> anyhow::Result<(
    ClientAdapterRegistration,
    DestinationEndpoint<LocalEncodedSourceTransport>,
    Vec<weld_core::host::ClientRuntimeWakeSource>,
)> {
    let EncodedSourceRegistrationOptions {
        upstream_source,
        adapter_source,
        destination_source,
        capabilities,
        codec,
        dump_directory,
        bitrate_budget,
    } = options;
    let (registration, endpoint, worker_wake) = registration::source_registration(
        LocalEncodedSourceTransport::new(control.clone(), media.clone()),
        destination_source,
        registration::SourceRegistrationOptions {
            upstream_source,
            adapter_source,
            capabilities,
            codec,
            dump_directory,
            bitrate_budget,
            gamepad: None,
        },
    )?;
    Ok((
        registration,
        endpoint,
        vec![
            control.runtime_wake_source(),
            media.runtime_wake_source(),
            worker_wake,
        ],
    ))
}

#[cfg(feature = "encoded-vaapi")]
pub fn encoded_destination_registration(
    control: LocalPacketConnection,
    media: LocalPacketConnection,
    upstream_source: ClientSourceId,
    destination_source: ClientSourceId,
    dmabuf: DmabufContext,
    capabilities: &weld_core::dmabuf::ExternalDmabufCapabilities,
    codec: weld_media::VideoCodec,
) -> anyhow::Result<(
    ClientAdapterRegistration,
    Vec<weld_core::host::ClientRuntimeWakeSource>,
)> {
    let (registration, worker_wake) = registration::destination_registration(
        LocalEncodedDestinationTransport::new(control.clone(), media.clone())?,
        upstream_source,
        destination_source,
        dmabuf,
        capabilities,
        codec,
    )?;
    Ok((
        registration,
        vec![
            control.runtime_wake_source(),
            media.runtime_wake_source(),
            worker_wake,
        ],
    ))
}
