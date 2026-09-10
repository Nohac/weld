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
    EncodedDestinationPort, EncodedSourcePort, EncoderRateControl, SharedBitrateBudget,
};

use crate::{
    LocalPacketConnection,
    destination::LocalDestinationPort,
    encoded_transport::{LocalEncodedDestinationTransport, LocalEncodedSourceTransport},
    source::LocalSourcePort,
};

#[cfg(feature = "encoded-vaapi")]
use weld_hoist_encoded::{decode_backend, encode_backend};

#[derive(Clone)]
pub struct LocalDestinationEndpoint {
    adapter_source: ClientSourceId,
    destination_source: ClientSourceId,
    connection: LocalPacketConnection,
    rate_control: Option<EncoderRateControl>,
}

impl LocalDestinationEndpoint {
    /// Optional source-side actuator; native-buffer hoisting has no encoder.
    pub fn encoder_rate_control(&self) -> Option<EncoderRateControl> {
        self.rate_control.clone()
    }
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
            rate_control: None,
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
) -> anyhow::Result<(ClientAdapterRegistration, LocalDestinationEndpoint)> {
    let descriptor = ClientSourceDescriptor::new(adapter_source, ClientProvenance::Relocated);
    let transport = LocalEncodedSourceTransport::new(control.clone(), media);
    let mut port = EncodedSourcePort::new(transport, backend);
    if let Some(budget) = bitrate_budget {
        port = port.with_bitrate_budget(budget)?;
    }
    let rate_control = port.encoder_rate_control();
    let adapter = SourceRelayAdapter::new(upstream_source, port);
    Ok((
        ClientAdapterRegistration::new(descriptor, adapter, ControlOnlyClientImporter),
        LocalDestinationEndpoint {
            adapter_source,
            destination_source,
            connection: control,
            rate_control,
        },
    ))
}

pub fn encoded_destination_registration_with_backend(
    control: LocalPacketConnection,
    media: LocalPacketConnection,
    upstream_source: ClientSourceId,
    destination_source: ClientSourceId,
    dmabuf: DmabufContext,
    backend: Box<dyn crate::LocalDecodeBackend>,
) -> anyhow::Result<ClientAdapterRegistration> {
    let descriptor = ClientSourceDescriptor::new(destination_source, ClientProvenance::Relocated);
    let transport = LocalEncodedDestinationTransport::new(control, media)?;
    let adapter = DestinationRelayAdapter::new(
        upstream_source,
        descriptor,
        EncodedDestinationPort::new(transport, backend, descriptor, dmabuf),
    );
    Ok(ClientAdapterRegistration::new(
        descriptor,
        adapter,
        DirectClientBufferImporter,
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
    LocalDestinationEndpoint,
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
    let (notifier, worker_wake) = weld_core::host::client_runtime_notifier()?;
    let backend = encode_backend(
        capabilities.render_node.clone(),
        codec,
        dump_directory.clone(),
        move || {
            if let Err(error) = notifier.notify() {
                tracing::error!(%error, "could not wake the host for encoded output");
            }
        },
    )?;
    let descriptor = ClientSourceDescriptor::new(adapter_source, ClientProvenance::Relocated);
    let transport = LocalEncodedSourceTransport::new(control.clone(), media.clone());
    let mut port = EncodedSourcePort::new(transport, backend);
    if let Some(budget) = bitrate_budget {
        port = port.with_bitrate_budget(budget)?;
    }
    if let Some(directory) = dump_directory {
        port = port.with_access_unit_dump_directory(directory, codec)?;
    }
    let rate_control = port.encoder_rate_control();
    let adapter = SourceRelayAdapter::new(upstream_source, port);
    let registration =
        ClientAdapterRegistration::new(descriptor, adapter, ControlOnlyClientImporter);
    let endpoint = LocalDestinationEndpoint {
        adapter_source,
        destination_source,
        connection: control.clone(),
        rate_control,
    };
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
    let (notifier, worker_wake) = weld_core::host::client_runtime_notifier()?;
    let backend = decode_backend(capabilities, codec, move || {
        if let Err(error) = notifier.notify() {
            tracing::error!(%error, "could not wake the host for decoded output");
        }
    })?;
    let registration = encoded_destination_registration_with_backend(
        control.clone(),
        media.clone(),
        upstream_source,
        destination_source,
        dmabuf,
        backend,
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
