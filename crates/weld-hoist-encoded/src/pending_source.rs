//! Install a source observer before pairing, then attach its real encoded port
//! only after authorization. There is no pre-admission media queue.

use std::{path::PathBuf, time::Instant};

use crate::{
    EncodeBackend, EncodedSourceOptions, EncodedSourcePort, EncodedSourceTransport,
    SharedBitrateBudget,
};
use weld_client::{
    ClientAdapterRegistration, ClientPresentationClaim, ClientProvenance, ClientSourceDescriptor,
    ClientSourceId, ClientSurfaceId, ControlOnlyClientImporter,
};
use weld_hoist_core::{
    HoistPortResult, HoistSourcePort, SourceAdmission, SourcePortCommand, SourceRelayAdapter,
};
use weld_hoist_protocol::DestinationEnvelope;
use weld_media::VideoCodec;

use crate::{PendingSource, SourceOptions};

#[cfg(test)]
#[path = "pending_source_tests.rs"]
mod tests;

/// Observe the source immediately, forwarding all mapped windows only after admission.
/// The supplied backend stays idle until authorization and owns no pre-admission queue.
pub fn pending_source_registration_with_backend<P: PendingSource>(
    pending: P,
    upstream: ClientSourceId,
    source: ClientSourceId,
    backend: Box<dyn EncodeBackend>,
    options: SourceOptions,
) -> ClientAdapterRegistration {
    ClientAdapterRegistration::new(
        ClientSourceDescriptor::new(source, ClientProvenance::Relocated),
        SourceRelayAdapter::with_admission(
            upstream,
            PendingPort {
                state: PortState::Waiting {
                    pending,
                    backend: Some(backend),
                },
                dump: options.dump_directory,
                budget: options.bitrate_budget,
            },
            SourceAdmission::AllToplevels,
        )
        .with_gamepad(options.gamepad),
        ControlOnlyClientImporter,
    )
}

enum PortState<P: PendingSource> {
    Waiting {
        pending: P,
        backend: Option<Box<dyn EncodeBackend>>,
    },
    Connected(Box<EncodedSourcePort<P::Transport>>),
    Closed,
}

struct PendingPort<P: PendingSource> {
    state: PortState<P>,
    dump: Option<(PathBuf, VideoCodec)>,
    budget: Option<SharedBitrateBudget>,
}

impl<P: PendingSource> HoistSourcePort for PendingPort<P> {
    fn next_deadline(&self) -> Option<Instant> {
        match &self.state {
            PortState::Connected(port) => port.next_deadline(),
            _ => None,
        }
    }
    fn set_presentation(
        &mut self,
        surface: ClientSurfaceId,
        claim: ClientPresentationClaim,
    ) -> HoistPortResult<ClientPresentationClaim> {
        match &mut self.state {
            PortState::Connected(port) => port.set_presentation(surface, claim),
            _ => Err(std::io::Error::other("presentation preceded authorization").into()),
        }
    }
    fn ready(&self) -> bool {
        matches!(self.state, PortState::Connected(_))
    }
    fn submit(&mut self, command: SourcePortCommand) -> HoistPortResult<()> {
        match &mut self.state {
            PortState::Connected(port) => port.submit(command),
            PortState::Waiting { .. }
                if matches!(command, SourcePortCommand::RetireUpstreamBuffer(_)) =>
            {
                Ok(())
            }
            _ => Err(std::io::Error::other("hoist source port is not authorized").into()),
        }
    }
    fn poll(&mut self) -> HoistPortResult<Vec<DestinationEnvelope>> {
        if let PortState::Waiting { pending, backend } = &mut self.state
            && let Some(peer) = pending.poll()?
        {
            let Some(backend) = backend.take() else {
                peer.disconnect();
                self.state = PortState::Closed;
                return Err(std::io::Error::other("pending encoder was already consumed").into());
            };
            let connected = EncodedSourcePort::configured(
                peer,
                backend,
                EncodedSourceOptions {
                    bitrate_budget: self.budget.take(),
                    access_unit_dump: self.dump.take(),
                },
            );
            match connected {
                Ok(port) => self.state = PortState::Connected(Box::new(port)),
                Err(error) => {
                    self.state = PortState::Closed;
                    return Err(error.into());
                }
            }
        }
        match &mut self.state {
            PortState::Waiting { .. } => Ok(Vec::new()),
            PortState::Connected(port) => port.poll(),
            PortState::Closed => Err(std::io::Error::other("hoist source port is closed").into()),
        }
    }
    fn accept_destination(&mut self, envelope: &DestinationEnvelope) -> HoistPortResult<()> {
        match &mut self.state {
            PortState::Connected(port) => port.accept_destination(envelope),
            _ => Err(std::io::Error::other("destination control preceded authorization").into()),
        }
    }
    fn progress_after_destination(&mut self) -> HoistPortResult<()> {
        match &mut self.state {
            PortState::Connected(port) => port.progress_after_destination(),
            _ => Ok(()),
        }
    }
    fn effects_drained(&mut self) {
        if let PortState::Connected(port) = &mut self.state {
            port.effects_drained();
        }
    }
    fn disconnect(&mut self) {
        if let PortState::Connected(mut port) =
            std::mem::replace(&mut self.state, PortState::Closed)
        {
            port.disconnect();
        }
    }
}

impl<P: PendingSource> Drop for PendingPort<P> {
    fn drop(&mut self) {
        self.disconnect();
    }
}
