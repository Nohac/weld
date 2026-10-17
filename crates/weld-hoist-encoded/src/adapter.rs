//! Client-adapter registrations over encoded connections.

use std::path::PathBuf;

use crate::{
    DecodeBackend, DecodedFramePublisher, EncodeBackend, EncodedDestinationPort,
    EncodedSourceOptions, EncodedSourcePort, SharedBitrateBudget,
};
use weld_client::{
    ClientAdapterCommandEnvelope, ClientAdapterRegistration, ClientProvenance,
    ClientSourceDescriptor, ClientSourceId, ClientSurfaceId, ControlOnlyClientImporter,
};
use weld_hoist_core::{
    DestinationRelayAdapter, HoistEndpoint, HoistEndpointCommand, HoistSessionId,
    SourceRelayAdapter, relocated_surface,
};
use weld_media::VideoCodec;

use crate::{EncodedDestinationTransport, SourceConnection};
use weld_hoist_core::gamepad::{GamepadController, GamepadProvider};

/// Trusted source assembly, independent of surface admission policy.
#[derive(Default)]
pub struct SourceOptions {
    pub dump_directory: Option<(PathBuf, VideoCodec)>,
    pub bitrate_budget: Option<SharedBitrateBudget>,
    pub gamepad: Option<Box<dyn GamepadProvider>>,
}

/// Source policy endpoint associated with one remote destination.
#[derive(Clone)]
pub struct DestinationEndpoint<T: SourceConnection> {
    adapter_source: ClientSourceId,
    destination_source: ClientSourceId,
    peer: T,
}

impl<T: SourceConnection> HoistEndpoint for DestinationEndpoint<T> {
    fn is_available(&self) -> bool {
        self.peer.is_connected()
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

pub fn source_registration_with_backend<T: SourceConnection>(
    peer: T,
    upstream_source: ClientSourceId,
    adapter_source: ClientSourceId,
    destination_source: ClientSourceId,
    backend: Box<dyn EncodeBackend>,
    options: SourceOptions,
) -> anyhow::Result<(ClientAdapterRegistration, DestinationEndpoint<T>)> {
    let descriptor = ClientSourceDescriptor::new(adapter_source, ClientProvenance::Relocated);
    let port = EncodedSourcePort::configured(
        peer.clone(),
        backend,
        EncodedSourceOptions {
            bitrate_budget: options.bitrate_budget,
            access_unit_dump: options.dump_directory,
        },
    )?;
    let adapter = SourceRelayAdapter::new(upstream_source, port).with_gamepad(options.gamepad);
    Ok((
        ClientAdapterRegistration::new(descriptor, adapter, ControlOnlyClientImporter),
        DestinationEndpoint {
            adapter_source,
            destination_source,
            peer,
        },
    ))
}

pub fn destination_registration_with_backend<
    P: DecodedFramePublisher,
    T: EncodedDestinationTransport + 'static,
>(
    peer: T,
    upstream_source: ClientSourceId,
    destination_source: ClientSourceId,
    publisher: P,
    backend: Box<dyn DecodeBackend<Output = P::Buffer>>,
    gamepad: Option<GamepadController>,
) -> ClientAdapterRegistration {
    let descriptor = ClientSourceDescriptor::new(destination_source, ClientProvenance::Relocated);
    let importer = publisher.client_importer();
    let diagnostics = peer.diagnostics();
    ClientAdapterRegistration::new(
        descriptor,
        DestinationRelayAdapter::new(
            upstream_source,
            descriptor,
            EncodedDestinationPort::new(peer, backend, descriptor, publisher),
        )
        .with_gamepad(gamepad),
        importer,
    )
    .with_diagnostics(diagnostics)
}
