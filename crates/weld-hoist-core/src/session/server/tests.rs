use super::*;
use crate::session::pairing::TrustStorage;
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Instant,
};
use weld_hoist_protocol::device::PairedDevice;

struct Storage;
impl TrustStorage for Storage {
    fn load(&self) -> Result<BTreeMap<String, PairedDevice>> {
        Ok(BTreeMap::new())
    }
    fn save(&self, _: &BTreeMap<String, PairedDevice>) -> Result<()> {
        Ok(())
    }
}
#[derive(Clone)]
struct Peer(Arc<AtomicBool>);
impl ConnectionControl for Peer {
    fn is_connected(&self) -> bool {
        !self.0.load(Ordering::Acquire)
    }
    fn close(&self) {
        self.0.store(true, Ordering::Release);
    }
}
struct TestConnection {
    peer: Peer,
    reads: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}
impl Drop for TestConnection {
    fn drop(&mut self) {
        self.peer.close();
        self.dropped.fetch_add(1, Ordering::AcqRel);
    }
}
struct Writer;
impl PacketSend for Writer {
    async fn send(&mut self, _: &[u8]) -> Result<()> {
        Ok(())
    }
    async fn finish(&mut self) -> Result<()> {
        Ok(())
    }
}
struct Reader(Arc<AtomicUsize>);
impl PacketReceive for Reader {
    async fn receive(&mut self, _: usize) -> Result<Vec<u8>> {
        self.0.fetch_add(1, Ordering::AcqRel);
        std::future::pending().await
    }
}
impl Connection for TestConnection {
    type Writer = Writer;
    type Reader = Reader;
    type Peer = Peer;
    type Control = Peer;
    fn identity(&self) -> Result<AuthenticatedPeer> {
        AuthenticatedPeer::new("test", "peer")
    }
    fn control(&self) -> Peer {
        self.peer.clone()
    }
    async fn closed(&self) -> Result<()> {
        std::future::pending().await
    }
    async fn accept_control(&self) -> Result<(Writer, Reader)> {
        Ok((Writer, Reader(self.reads.clone())))
    }
    async fn open_source(
        &self,
        _: SourcePresentation,
        _: Wake,
    ) -> Result<SourceChannels<Peer, Writer, Reader>> {
        anyhow::bail!("this fixture exercises enrollment")
    }
}
struct FirstListener(Receiver<Accepted<TestConnection>>);
struct SecondListener(Receiver<Accepted<TestConnection>>);
impl Listener for FirstListener {
    type Connection = TestConnection;
    async fn accept(&mut self) -> Option<Result<Accepted<TestConnection>>> {
        self.0.recv().await.ok().map(Ok)
    }
}
impl Listener for SecondListener {
    type Connection = TestConnection;
    async fn accept(&mut self) -> Option<Result<Accepted<TestConnection>>> {
        self.0.recv().await.ok().map(Ok)
    }
}
#[derive(Clone)]
struct Host(PairingAuthority);
impl Services<TestConnection> for Host {
    fn authority(&self) -> Result<PairingAuthority> {
        Ok(self.0.clone())
    }
    fn source(&self, _: &TestConnection) -> Result<SourceContext<Peer>> {
        anyhow::bail!("no media fixture")
    }
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "session runtime did not progress"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn distinct_listeners_share_task_limits_and_shutdown_retires_all_clients() {
    let runtime = SessionRuntime::start().expect("runtime");
    let host = Host(PairingAuthority::start("test".into(), || Ok(Storage)).expect("authority"));
    let (first, first_incoming) = async_channel::bounded(16);
    let (second, second_incoming) = async_channel::bounded(16);
    let direct = runtime
        .listen(FirstListener(first_incoming), host.clone())
        .expect("first listener");
    let _second_direct = runtime
        .listen(SecondListener(second_incoming), host)
        .expect("second listener");
    let reads = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let connection = || TestConnection {
        peer: Peer(Arc::new(AtomicBool::new(false))),
        reads: reads.clone(),
        dropped: dropped.clone(),
    };
    for index in 0..8 {
        let sender = if index % 2 == 0 { &first } else { &second };
        sender
            .try_send(Accepted {
                protocol: Protocol::Enrollment,
                connection: connection(),
            })
            .expect("enqueue");
    }
    wait(|| reads.load(Ordering::Acquire) == 8);
    second
        .try_send(Accepted {
            protocol: Protocol::Enrollment,
            connection: connection(),
        })
        .expect("overload");
    wait(|| dropped.load(Ordering::Acquire) == 1);
    assert_eq!(
        reads.load(Ordering::Acquire),
        8,
        "limits apply across listeners"
    );

    first
        .try_send(Accepted {
            protocol: Protocol::DirectSource,
            connection: connection(),
        })
        .expect("direct");
    wait(|| !direct.is_empty());
    drop(direct.try_recv().expect("direct stays independent"));
    first
        .try_send(Accepted {
            protocol: Protocol::DirectSource,
            connection: connection(),
        })
        .expect("unclaimed");
    wait(|| !direct.is_empty());
    drop(runtime);
    assert_eq!(dropped.load(Ordering::Acquire), 11);
    assert!(direct.is_empty());
    assert!(direct.is_closed());
    assert!(first.is_closed());
    assert!(second.is_closed());
}

#[test]
fn cancelled_admission_retires_permit_before_session_id_is_published() {
    let source =
        SourceSessions::<Peer>::new(|| Ok(()), || Ok(()), VideoCodec::H264).expect("source");
    let access = source.access.open("test".into()).expect("permit");
    let joining = JoinedSession {
        source: source.clone(),
        id: None,
        access: access.clone(),
    };
    let id = futures_lite::future::block_on(
        source
            .services
            .join(access.clone(), DevicePermissions::default())
            .expect("join")
            .wait(),
    )
    .expect("reply")
    .expect("joined");
    drop(joining);
    assert!(!access.is_authorized());
    assert!(futures_lite::future::block_on(source.services.subscribe(id)).is_err());
}
