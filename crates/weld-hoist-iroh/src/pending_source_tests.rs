use std::{
    cell::Cell,
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

use weld_client::{ClientBufferId, ClientBufferLease, ClientId, ClientSurfaceId};
use weld_hoist_encoded::{
    EncodeCompletion, EncodeRequest, EncodedDestinationTransport, PreparedEncodeInput,
    ReceiveBudget, SubmitError,
};
use weld_hoist_protocol::HoistSessionId;
use weld_media::{MediaStreamId, StreamGeneration};

use super::*;
use crate::{IrohHost, IrohNetwork, IrohNotifier, rendezvous::tests::ExchangeDirectory};

struct IdleEncoder(Rc<Cell<usize>>);

impl EncodeBackend for IdleEncoder {
    fn prepare_input(&self, _lease: &ClientBufferLease) -> anyhow::Result<PreparedEncodeInput> {
        self.0.set(self.0.get() + 1);
        anyhow::bail!("test encoder cannot encode")
    }
    fn try_submit(&mut self, request: EncodeRequest) -> Result<(), SubmitError<EncodeRequest>> {
        self.0.set(self.0.get() + 1);
        Err(SubmitError::Stopped(request))
    }
    fn drain(&mut self) -> Vec<EncodeCompletion> {
        self.0.set(self.0.get() + 1);
        Vec::new()
    }
    fn retire(
        &mut self,
        _stream: MediaStreamId,
        _generation: StreamGeneration,
    ) -> anyhow::Result<()> {
        self.0.set(self.0.get() + 1);
        Ok(())
    }
}

#[test]
fn waiting_port_discards_retirement_but_rejects_mapping_without_touching_encoder() {
    let directory = ExchangeDirectory::new();
    let host = IrohHost::bind(IrohNetwork::Direct).expect("host");
    let pending = host
        .begin_accept_source(
            directory.0.join("source.ticket"),
            directory.0.join("missing.identity"),
            VideoCodec::Av1,
            IrohNotifier::new(|| Ok(())),
            Duration::from_secs(5),
        )
        .expect("pending");
    let calls = Rc::new(Cell::new(0));
    let mut port = PendingPort {
        state: PortState::Waiting {
            pending,
            backend: Some(Box::new(IdleEncoder(calls.clone()))),
        },
        dump: None,
        budget: None,
    };
    let source = ClientSourceId::new(1);
    assert!(!port.ready());
    assert!(port.next_deadline().is_none());
    assert!(
        port.set_presentation(
            ClientSurfaceId::new(ClientId::new(source, 1), 1),
            ClientPresentationClaim::Active { rate: None }
        )
        .is_err()
    );
    port.submit(SourcePortCommand::RetireUpstreamBuffer(
        ClientBufferId::new(source, 1),
    ))
    .expect("local retirement");
    assert!(
        port.submit(SourcePortCommand::MapSurface {
            session: HoistSessionId::new(1),
            surface: ClientSurfaceId::new(ClientId::new(source, 1), 1),
        })
        .is_err()
    );
    assert!(port.poll().expect("poll pending").is_empty());
    assert_eq!(calls.get(), 0);
    port.disconnect();
    assert!(!port.ready());
    assert!(port.poll().is_err());
}

#[test]
fn failure_configuring_connected_port_closes_the_claimed_peer() {
    let directory = ExchangeDirectory::new();
    let host = IrohHost::bind(IrohNetwork::Direct).expect("source");
    let destination = IrohHost::bind(IrohNetwork::Direct).expect("destination");
    let identity = directory.0.join("destination.identity");
    let ticket = directory.0.join("source.ticket");
    destination.publish_identity(&identity).expect("identity");
    let pending = host
        .begin_accept_source(
            &ticket,
            &identity,
            VideoCodec::Av1,
            IrohNotifier::new(|| Ok(())),
            Duration::from_secs(5),
        )
        .expect("pending");
    let peer = destination
        .connect_destination(
            &ticket,
            vec![VideoCodec::Av1],
            IrohNotifier::new(|| Ok(())),
            Duration::from_secs(5),
        )
        .expect("destination");
    let mut port = PendingPort {
        state: PortState::Waiting {
            pending,
            backend: Some(Box::new(IdleEncoder(Rc::new(Cell::new(0))))),
        },
        dump: Some((identity, VideoCodec::Av1)), // Existing regular file is not a dump directory.
        budget: None,
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while port.poll().is_ok() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!port.ready());
    while peer.drain(ReceiveBudget::ALL).is_ok() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
}
