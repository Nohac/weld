use super::*;
use async_channel::{Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};

struct Writer(Sender<Vec<u8>>);
struct Reader(Receiver<Vec<u8>>);
impl PacketSend for Writer {
    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.0.send(bytes.to_vec()).await?;
        Ok(())
    }
    async fn finish(&mut self) -> Result<()> {
        Ok(())
    }
}
impl PacketReceive for Reader {
    async fn receive(&mut self, maximum: usize) -> Result<Vec<u8>> {
        let bytes = self.0.recv().await?;
        ensure!(bytes.len() <= maximum, "packet too large");
        Ok(bytes)
    }
}
struct LocalConnection {
    name: String,
    outgoing: Sender<Vec<u8>>,
    incoming: Receiver<Vec<u8>>,
    closed: Arc<AtomicBool>,
}
impl Drop for LocalConnection {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
}
struct Peer {
    _connection: LocalConnection,
    presentation: Presentation,
}
impl ConnectionControl for Peer {
    fn is_connected(&self) -> bool {
        !self._connection.closed.load(Ordering::Acquire)
    }
    fn close(&self) {
        self._connection.closed.store(true, Ordering::Release);
    }
}
impl Connection for LocalConnection {
    type Writer = Writer;
    type Reader = Reader;
    type Source = Peer;
    type Destination = Peer;
    fn native_buffers(&self) -> bool {
        true
    }
    fn identity(&self) -> Result<AuthenticatedPeer> {
        AuthenticatedPeer::new("test", self.name.clone())
    }
    async fn open(&self) -> Result<(Writer, Reader)> {
        Ok((Writer(self.outgoing.clone()), Reader(self.incoming.clone())))
    }
    async fn accept(&self) -> Result<(Writer, Reader)> {
        self.open().await
    }
    async fn source(
        self,
        _: (Writer, Reader),
        presentation: Presentation,
        _: Wake,
    ) -> Result<Peer> {
        Ok(Peer {
            _connection: self,
            presentation,
        })
    }
    async fn destination(
        self,
        _: (Writer, Reader),
        presentation: Presentation,
        _: Wake,
    ) -> Result<Peer> {
        Ok(Peer {
            _connection: self,
            presentation,
        })
    }
}
struct Fixture {
    connection: LocalConnection,
    packets: Receiver<Vec<u8>>,
    replies: Sender<Vec<u8>>,
    closed: Arc<AtomicBool>,
}
fn connection(name: &str) -> Fixture {
    let (outgoing, packets) = async_channel::bounded(8);
    let (replies, incoming) = async_channel::bounded(8);
    let closed = Arc::new(AtomicBool::new(false));
    Fixture {
        connection: LocalConnection {
            name: name.into(),
            outgoing,
            incoming,
            closed: closed.clone(),
        },
        packets,
        replies,
        closed,
    }
}

#[tokio::test]
async fn unauthorized_and_stalled_candidates_do_not_consume_the_approved_admission() {
    let (sender, incoming) = async_channel::bounded(8);
    let wrong = connection("intruder");
    let stalled = connection("approved");
    let correct = connection("approved");
    send_record(
        &mut Writer(correct.replies),
        &BootstrapAnswer {
            revision: ProtocolRevision::CURRENT,
            role: PeerRole::Destination,
            rejection: None,
            stream_mode: SurfaceStreamMode::Independent,
        },
    )
    .await
    .expect("answer");
    assert!(sender.send(wrong.connection).await.is_ok());
    assert!(sender.send(stalled.connection).await.is_ok());
    assert!(sender.send(correct.connection).await.is_ok());
    let peer = accept(
        incoming,
        vec![AuthenticatedPeer::new("test", "approved").expect("identity")],
        SurfaceMode::Native,
        Instant::now() + Duration::from_secs(1),
        Arc::new(|| {}),
    )
    .await
    .expect("approved candidate");
    assert_eq!(peer.presentation.mode, SurfaceMode::Native);
    assert!(wrong.closed.load(Ordering::Acquire));
    assert!(wrong.packets.is_empty(), "unauthorized peer saw metadata");
    assert!(
        stalled.closed.load(Ordering::Acquire),
        "losing candidate was retained"
    );
}

#[tokio::test]
async fn native_delivery_needs_no_codec_and_rejection_is_returned_to_the_source() {
    for native_buffers in [true, false] {
        let (send_offer, read_offer) = async_channel::bounded(1);
        let (send_answer, read_answer) = async_channel::bounded(1);
        let mut source_writer = Writer(send_offer);
        let mut source_reader = Reader(read_answer);
        let source = source_exchange(&mut source_writer, &mut source_reader, SurfaceMode::Native);
        let delivery = Delivery {
            native_buffers,
            encoded: ReceiverPreferences::from(Vec::new()),
        };
        let mut destination_writer = Writer(send_answer);
        let mut destination_reader = Reader(read_offer);
        let destination =
            destination_exchange(&mut destination_writer, &mut destination_reader, &delivery);
        let (source, destination) = tokio::join!(source, destination);
        assert_eq!(source.is_ok(), native_buffers);
        assert_eq!(destination.is_ok(), native_buffers);
    }
}

#[tokio::test]
async fn admission_deadline_retires_a_silent_candidate() {
    let (sender, incoming) = async_channel::bounded(1);
    let fixture = connection("approved");
    assert!(sender.send(fixture.connection).await.is_ok());
    assert!(
        accept(
            incoming,
            vec![AuthenticatedPeer::new("test", "approved").expect("identity")],
            SurfaceMode::Native,
            Instant::now() + Duration::from_millis(20),
            Arc::new(|| {})
        )
        .await
        .is_err()
    );
    assert!(fixture.closed.load(Ordering::Acquire));
}

struct FixedConnector(LocalConnection);
impl Connector for FixedConnector {
    type Connection = LocalConnection;
    async fn connect(self, protocol: &'static str) -> Result<LocalConnection> {
        ensure!(protocol == DIRECT_SERVICE, "unexpected service");
        Ok(self.0)
    }
}
#[tokio::test]
async fn destination_deadline_retires_a_source_that_never_offers() {
    let fixture = connection("source");
    assert!(
        connect(
            FixedConnector(fixture.connection),
            Delivery {
                native_buffers: true,
                encoded: ReceiverPreferences::from(Vec::new()),
            },
            Instant::now() + Duration::from_millis(20),
            Arc::new(|| {})
        )
        .await
        .is_err()
    );
    assert!(fixture.closed.load(Ordering::Acquire));
}
