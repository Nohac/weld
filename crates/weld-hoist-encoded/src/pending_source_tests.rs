use super::*;
use crate::{
    EncodeCompletion, EncodeRequest, PreparedEncodeInput, SendStatus, SourceConnection,
    SourceTransportPacket, SubmitError,
};
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use weld_client::{ClientBufferId, ClientBufferLease, ClientId};
use weld_hoist_core::session::pairing::ConnectionControl;
use weld_hoist_protocol::HoistSessionId;
use weld_media::{MediaStreamId, StreamGeneration};

#[derive(Clone)]
struct Connection(Arc<AtomicBool>);
impl SourceConnection for Connection {}
impl weld_hoist_core::session::pairing::ConnectionControl for Connection {
    fn close(&self) {
        self.disconnect();
    }
    fn is_connected(&self) -> bool {
        !self.0.load(Ordering::Relaxed)
    }
}
impl EncodedSourceTransport for Connection {
    fn try_send(
        &self,
        packet: SourceTransportPacket,
    ) -> HoistPortResult<SendStatus<SourceTransportPacket>> {
        Ok(SendStatus::Busy(packet))
    }
    fn media_headroom(&self) -> bool {
        false
    }
    fn drain(&self) -> HoistPortResult<Vec<DestinationEnvelope>> {
        Ok(Vec::new())
    }
    fn disconnect(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
struct Admission(Option<Connection>);
impl PendingSource for Admission {
    type Transport = Connection;
    fn poll(&mut self) -> anyhow::Result<Option<Connection>> {
        Ok(self.0.take())
    }
}
struct IdleEncoder(Rc<Cell<usize>>);
impl EncodeBackend for IdleEncoder {
    fn prepare_input(&self, _: &ClientBufferLease) -> anyhow::Result<PreparedEncodeInput> {
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
    fn retire(&mut self, _: MediaStreamId, _: StreamGeneration) -> anyhow::Result<()> {
        self.0.set(self.0.get() + 1);
        Ok(())
    }
}

#[test]
fn waiting_port_rejects_mapping_without_touching_encoder() {
    let calls = Rc::new(Cell::new(0));
    let mut port = PendingPort {
        state: PortState::Waiting {
            pending: Admission(None),
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
    .expect("retirement");
    assert!(
        port.submit(SourcePortCommand::MapSurface {
            session: HoistSessionId::new(1),
            surface: ClientSurfaceId::new(ClientId::new(source, 1), 1),
        })
        .is_err()
    );
    assert!(port.poll().expect("waiting").is_empty());
    assert_eq!(calls.get(), 0);
    port.disconnect();
    assert!(!port.ready());
    assert!(port.poll().is_err());
}

#[test]
fn failed_port_configuration_disconnects_admitted_transport() {
    let connection = Connection(Arc::new(AtomicBool::new(false)));
    let mut port = PendingPort {
        state: PortState::Waiting {
            pending: Admission(Some(connection.clone())),
            backend: Some(Box::new(IdleEncoder(Rc::new(Cell::new(0))))),
        },
        // An existing file cannot become a dump directory.
        dump: Some((
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
            VideoCodec::Av1,
        )),
        budget: None,
    };
    assert!(port.poll().is_err());
    assert!(!connection.is_connected());
    assert!(!port.ready());
}

#[test]
fn queued_admissions_retire_on_revoke_and_owner_drop() {
    let queue = weld_hoist_core::session::admission::SourceAdmissions::default();
    let authority = weld_access::AccessAuthority::new(|| {});
    let first = Connection(Arc::new(AtomicBool::new(false)));
    let access = authority.open("first".into()).expect("access");
    queue
        .admit(
            weld_hoist_core::session::SessionId(1),
            first.clone(),
            access.clone(),
            VideoCodec::Av1,
        )
        .expect("admit");
    access.end();
    let second = Connection(Arc::new(AtomicBool::new(false)));
    let access = authority.open("second".into()).expect("access");
    queue
        .admit(
            weld_hoist_core::session::SessionId(2),
            second.clone(),
            access.clone(),
            VideoCodec::H264,
        )
        .expect("admit");
    assert!(!first.is_connected());
    assert!(second.is_connected());
    drop(queue);
    assert!(!second.is_connected());
    assert!(!access.is_authorized());
}
