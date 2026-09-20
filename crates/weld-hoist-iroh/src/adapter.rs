//! Client-adapter registrations over one encoded Iroh peer.

use std::path::PathBuf;

use weld_client::{
    ClientAdapterCommandEnvelope, ClientAdapterRegistration, ClientProvenance,
    ClientSourceDescriptor, ClientSourceId, ClientSurfaceId, ControlOnlyClientImporter,
};
use weld_hoist_core::{
    DestinationRelayAdapter, HoistEndpoint, HoistEndpointCommand, HoistSessionId,
    SourceRelayAdapter, relocated_surface,
};
use weld_hoist_encoded::{
    DecodeBackend, DecodedFramePublisher, EncodeBackend, EncodedDestinationPort,
    EncodedSourceOptions, EncodedSourcePort, EncoderRateControl, SharedBitrateBudget,
};
use weld_media::VideoCodec;

use crate::{IrohDestinationPeer, IrohSourcePeer};
use weld_hoist_core::gamepad::{GamepadController, GamepadProvider};

/// Trusted source assembly, independent of surface admission policy.
#[derive(Default)]
pub struct IrohSourceOptions {
    pub dump_directory: Option<(PathBuf, VideoCodec)>,
    pub bitrate_budget: Option<SharedBitrateBudget>,
    pub gamepad: Option<Box<dyn GamepadProvider>>,
}

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

pub fn source_registration_with_backend(
    peer: IrohSourcePeer,
    upstream_source: ClientSourceId,
    adapter_source: ClientSourceId,
    destination_source: ClientSourceId,
    backend: Box<dyn EncodeBackend>,
    options: IrohSourceOptions,
) -> anyhow::Result<(ClientAdapterRegistration, IrohDestinationEndpoint)> {
    let descriptor = ClientSourceDescriptor::new(adapter_source, ClientProvenance::Relocated);
    let port = EncodedSourcePort::configured(
        peer.clone(),
        backend,
        EncodedSourceOptions {
            bitrate_budget: options.bitrate_budget,
            access_unit_dump: options.dump_directory,
        },
    )?;
    let rate_control = port.encoder_rate_control();
    let adapter = SourceRelayAdapter::new(upstream_source, port).with_gamepad(options.gamepad);
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

pub fn destination_registration_with_backend<P: DecodedFramePublisher>(
    peer: IrohDestinationPeer,
    upstream_source: ClientSourceId,
    destination_source: ClientSourceId,
    publisher: P,
    backend: Box<dyn DecodeBackend<Output = P::Buffer>>,
    gamepad: Option<GamepadController>,
) -> ClientAdapterRegistration {
    let descriptor = ClientSourceDescriptor::new(destination_source, ClientProvenance::Relocated);
    let importer = publisher.client_importer();
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
}
