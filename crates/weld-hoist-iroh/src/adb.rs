//! Private packet routing over launcher-owned ADB TCP tunnels.
//!
//! Route addresses name local socket lifetimes, never peer identities. Normal
//! Iroh TLS and Weld admission authorize every connection before promotion.
//! A failed socket must not fail the persistent, custom-only Iroh endpoint.
use anyhow::{Context as _, Result, ensure};
use iroh::{
    EndpointId,
    endpoint::{
        Connection,
        transports::{CustomEndpoint, CustomSender, CustomTransport, RecvInfo, Transmit},
    },
};
use iroh_base::CustomAddr;
use n0_watcher::{Direct, Watchable};
use std::{
    collections::BTreeMap,
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch},
    task::JoinHandle,
};

pub(crate) const TRANSPORT_ID: u64 = 0x77656c64616462;
const MAX_LINKS: usize = 8;
const SEND_CAPACITY: usize = 256;
const RECEIVE_CAPACITY: usize = 32;
const MAX_PACKET: usize = u16::MAX as usize;
const ADMISSION_GRACE: Duration = Duration::from_secs(10);

#[cfg(test)]
#[path = "adb_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Pending,
    Admitted,
    Closed,
}

#[derive(Debug)]
struct Link {
    send: mpsc::Sender<Vec<u8>>,
    state: watch::Sender<State>,
    done: watch::Sender<bool>,
    sent: AtomicU64,
    received: AtomicU64,
    dropped: AtomicU64,
}

impl Link {
    fn enqueue(&self, contents: &[u8]) -> io::Result<()> {
        if contents.is_empty() || contents.len() > MAX_PACKET {
            return Err(io::Error::other("invalid ADB packet size"));
        }
        match self.send.try_reserve() {
            Ok(permit) => {
                let length = u16::try_from(contents.len()).map_err(io::Error::other)?;
                let mut bytes = Vec::with_capacity(contents.len() + 2);
                bytes.extend_from_slice(&length.to_be_bytes());
                bytes.extend_from_slice(contents);
                permit.send(bytes);
                self.sent.fetch_add(1, Ordering::Relaxed);
            }
            // Iroh 1.1.0 swallows Pending. Explicitly count packet loss;
            // never claim to apply backpressure or drop application records.
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(io::Error::other("ADB writer closed"));
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Packet {
    route: u64,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct Table {
    next: u64,
    links: BTreeMap<u64, Arc<Link>>,
}

#[derive(Debug)]
pub(crate) struct Manager {
    table: Mutex<Table>,
    slots: Arc<Semaphore>,
    // Keep this sender alive even with no sockets: empty receive is Pending,
    // not a fatal custom transport error that prevents subsequent redials.
    incoming: mpsc::Sender<Packet>,
    receiver: Mutex<Option<mpsc::Receiver<Packet>>>,
    local: CustomAddr,
}

pub(crate) struct Tasks(Vec<JoinHandle<()>>);
impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

impl Manager {
    pub fn new(identity: EndpointId) -> Arc<Self> {
        let (incoming, receiver) = mpsc::channel(RECEIVE_CAPACITY);
        let mut data = vec![0];
        data.extend_from_slice(identity.as_bytes());
        Arc::new(Self {
            table: Mutex::new(Table {
                next: 1,
                links: BTreeMap::new(),
            }),
            slots: Arc::new(Semaphore::new(MAX_LINKS)),
            incoming,
            receiver: Mutex::new(Some(receiver)),
            local: CustomAddr::from_parts(TRANSPORT_ID, &data),
        })
    }

    pub fn serve(self: &Arc<Self>, listener: Option<TcpListener>) -> Tasks {
        let mut tasks = Vec::new();
        if let Some(listener) = listener {
            let manager = self.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    let stream = match listener.accept().await {
                        Ok((stream, _)) => stream,
                        Err(error) => {
                            tracing::warn!(%error, "ADB listener stopped");
                            break;
                        }
                    };
                    let result = async {
                        let slot = manager.reserve().await?;
                        manager.attach(stream, slot)
                    }
                    .await;
                    match result {
                        Ok(mut lease) => lease.armed = false, // pending grace owns this socket
                        Err(error) => tracing::debug!(%error, "rejected an ADB socket"),
                    }
                }
            }));
        }
        let manager = Arc::downgrade(self);
        tasks.push(tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(5));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                let Some(manager) = manager.upgrade() else {
                    break;
                };
                if let Ok(table) = manager.table.lock() {
                    for (route, link) in &table.links {
                        tracing::debug!(target: "weld_network_diag", route,
                            packets_enqueued_total = link.sent.load(Ordering::Relaxed),
                            packets_received_total = link.received.load(Ordering::Relaxed),
                            adapter_dropped_total = link.dropped.load(Ordering::Relaxed),
                            "Iroh ADB link observations");
                    }
                }
            }
        }));
        Tasks(tasks)
    }

    async fn reserve(&self) -> Result<OwnedSemaphorePermit> {
        loop {
            if let Ok(slot) = self.slots.clone().try_acquire_owned() {
                return Ok(slot);
            }
            let mut done = {
                let table = self
                    .table
                    .lock()
                    .map_err(|_| io::Error::other("ADB table poisoned"))?;
                let link = table
                    .links
                    .values()
                    .find(|link| *link.state.borrow() == State::Pending)
                    .context(
                        "all ADB link slots are occupied by admitted peers or pending dials",
                    )?;
                let done = link.done.subscribe();
                link.state.send_replace(State::Closed);
                done
            };
            // Slots include cancelled drivers until their buffers/socket are gone.
            while !*done.borrow_and_update() {
                done.changed().await?;
            }
        }
    }

    pub async fn connect(self: &Arc<Self>, address: SocketAddr) -> Result<Lease> {
        ensure!(
            address.ip().is_loopback() && address.port() != 0,
            "ADB target must be nonzero loopback TCP"
        );
        let slot = self.reserve().await?;
        let socket = TcpStream::connect(address)
            .await
            .context("could not connect ADB TCP tunnel")?;
        self.attach(socket, slot)
    }

    fn attach(self: &Arc<Self>, socket: TcpStream, slot: OwnedSemaphorePermit) -> Result<Lease> {
        socket.set_nodelay(true)?;
        let (send, outgoing) = mpsc::channel(SEND_CAPACITY);
        let link = Arc::new(Link {
            send,
            state: watch::channel(State::Pending).0,
            done: watch::channel(false).0,
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        let route = {
            let mut table = self
                .table
                .lock()
                .map_err(|_| io::Error::other("ADB table poisoned"))?;
            let route = table.next;
            table.next = route.checked_add(1).context("ADB route IDs exhausted")?;
            table.links.insert(route, link.clone());
            route
        };
        let cleanup = Cleanup {
            manager: Arc::downgrade(self),
            route,
            link: link.clone(),
            slot: Some(slot),
        };
        let incoming = self.incoming.clone();
        tokio::spawn(async move {
            let _cleanup = cleanup;
            let (mut read, mut write) = socket.into_split();
            let mut outgoing = outgoing;
            let reading = async {
                while let Some(bytes) = read_packet(&mut read).await? {
                    _cleanup.link.received.fetch_add(1, Ordering::Relaxed);
                    incoming
                        .send(Packet { route, bytes })
                        .await
                        .map_err(|_| io::Error::other("ADB receiver stopped"))?;
                }
                Err::<(), io::Error>(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "ADB tunnel closed",
                ))
            };
            let writing = async {
                while let Some(packet) = outgoing.recv().await {
                    // Length and payload were framed together before enqueue.
                    // Avoid the probe's separate tiny header TCP write.
                    write.write_all(&packet).await?;
                }
                Err::<(), io::Error>(io::Error::other("ADB writer stopped"))
            };
            let mut state = _cleanup.link.state.subscribe();
            let stop = async {
                let deadline = tokio::time::sleep(ADMISSION_GRACE);
                tokio::pin!(deadline);
                loop {
                    let current = *state.borrow_and_update();
                    if current == State::Closed {
                        break;
                    }
                    tokio::select! {
                        result = state.changed() => { if result.is_err() { break; } },
                        _ = &mut deadline, if current == State::Pending => break,
                    }
                }
            };
            tokio::select! {
                biased;
                _ = stop => {},
                result = async { tokio::try_join!(reading, writing) } => {
                    if let Err(error) = result { tracing::debug!(route, %error, "ADB link ended"); }
                }
            }
        });
        Ok(Lease {
            route,
            link,
            armed: true,
        })
    }

    pub fn admitted(&self, address: &CustomAddr, connection: &Connection) -> Result<()> {
        let route = route_id(address).context("invalid admitted ADB route")?;
        let link = self
            .table
            .lock()
            .map_err(|_| io::Error::other("ADB table poisoned"))?
            .links
            .get(&route)
            .cloned()
            .context("admitted ADB socket already closed")?;
        Lease {
            route,
            link,
            armed: true,
        }
        .watch(connection)
    }

    pub fn close(&self) {
        self.slots.close();
        if let Ok(table) = self.table.lock() {
            for link in table.links.values() {
                link.state.send_replace(State::Closed);
            }
        }
    }
}

struct Cleanup {
    manager: Weak<Manager>,
    route: u64,
    link: Arc<Link>,
    slot: Option<OwnedSemaphorePermit>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        self.link.state.send_replace(State::Closed);
        if let Some(manager) = self.manager.upgrade()
            && let Ok(mut table) = manager.table.lock()
        {
            table.links.remove(&self.route);
        }
        drop(self.slot.take());
        self.link.done.send_replace(true);
    }
}

pub(crate) struct Lease {
    route: u64,
    link: Arc<Link>,
    armed: bool,
}
impl Lease {
    pub fn address(&self) -> CustomAddr {
        route_addr(self.route)
    }
    pub fn watch(mut self, connection: &Connection) -> Result<()> {
        if !self.link.state.send_if_modified(|state| {
            if *state != State::Pending {
                return false;
            }
            *state = State::Admitted;
            true
        }) {
            self.armed = false;
            anyhow::bail!("ADB route is closed or already admitted");
        }
        self.armed = true;
        let connection = connection.weak_handle();
        tokio::spawn(async move {
            let mut state = self.link.state.subscribe();
            let closed = connection.closed();
            tokio::pin!(closed);
            loop {
                if *state.borrow_and_update() == State::Closed {
                    if let Some(connection) = connection.upgrade() {
                        connection.close(1_u32.into(), b"ADB link disconnected");
                    }
                    break;
                }
                tokio::select! {
                    _ = &mut closed => break,
                    result = state.changed() => { if result.is_err() { break; } },
                }
            }
            drop(self);
        });
        Ok(())
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if self.armed {
            self.link.state.send_replace(State::Closed);
        }
    }
}

fn route_addr(route: u64) -> CustomAddr {
    let mut bytes = [1; 9];
    bytes[1..].copy_from_slice(&route.to_be_bytes());
    CustomAddr::from_parts(TRANSPORT_ID, &bytes)
}
fn route_id(address: &CustomAddr) -> Option<u64> {
    let data = address.data();
    if address.id() != TRANSPORT_ID || data.len() != 9 || data.first() != Some(&1) {
        return None;
    }
    Some(u64::from_be_bytes(data.get(1..)?.try_into().ok()?))
}

async fn read_packet(read: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0; 2];
    if read.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    read.read_exact(&mut header[1..]).await?;
    let length = usize::from(u16::from_be_bytes(header));
    if length == 0 {
        return Err(io::Error::other("empty ADB packet"));
    }
    let mut bytes = vec![0; length];
    read.read_exact(&mut bytes).await?;
    Ok(Some(bytes))
}

#[derive(Debug)]
pub(crate) struct Transport(pub Arc<Manager>);
impl CustomTransport for Transport {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
        let receiver = self
            .0
            .receiver
            .lock()
            .map_err(|_| io::Error::other("ADB receiver poisoned"))?
            .take()
            .ok_or_else(|| io::Error::other("ADB transport already bound"))?;
        Ok(Box::new(PacketEndpoint {
            receiver,
            sender: self.0.clone(),
            local: Watchable::new(vec![self.0.local.clone()]),
        }))
    }
}

impl CustomSender for Manager {
    fn is_valid_send_addr(&self, address: &CustomAddr) -> bool {
        route_id(address)
            .and_then(|route| self.table.lock().ok()?.links.get(&route).cloned())
            .is_some_and(|link| *link.state.borrow() != State::Closed)
    }
    fn poll_send(
        &self,
        _: &mut Context<'_>,
        destination: &CustomAddr,
        _: Option<&CustomAddr>,
        packet: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        let result = (|| {
            let route =
                route_id(destination).ok_or_else(|| io::Error::other("invalid ADB route"))?;
            let link = self
                .table
                .lock()
                .map_err(|_| io::Error::other("ADB table poisoned"))?
                .links
                .get(&route)
                .cloned()
                .ok_or_else(|| io::Error::other("retired ADB route"))?;
            if packet
                .segment_size
                .is_some_and(|size| size < packet.contents.len())
            {
                return Err(io::Error::other("unsupported ADB packet segmentation"));
            }
            link.enqueue(packet.contents)
        })();
        Poll::Ready(result)
    }
}

#[derive(Debug)]
struct PacketEndpoint {
    receiver: mpsc::Receiver<Packet>,
    sender: Arc<Manager>,
    local: Watchable<Vec<CustomAddr>>,
}
impl CustomEndpoint for PacketEndpoint {
    fn watch_local_addrs(&self) -> Direct<Vec<CustomAddr>> {
        self.local.watch()
    }
    fn create_sender(&self) -> Arc<dyn CustomSender> {
        self.sender.clone()
    }
    fn poll_recv(
        &mut self,
        context: &mut Context<'_>,
        buffers: &mut [io::IoSliceMut<'_>],
        metadata: &mut [noq_udp::RecvMeta],
        infos: &mut [RecvInfo],
    ) -> Poll<io::Result<usize>> {
        let mut count = 0;
        for ((buffer, meta), info) in buffers.iter_mut().zip(metadata).zip(infos) {
            match self.receiver.poll_recv(context) {
                Poll::Ready(Some(packet)) if packet.bytes.len() <= buffer.len() => {
                    buffer[..packet.bytes.len()].copy_from_slice(&packet.bytes);
                    *meta = noq_udp::RecvMeta::default();
                    meta.len = packet.bytes.len();
                    meta.stride = packet.bytes.len();
                    *info =
                        RecvInfo::new(route_addr(packet.route), self.local.get().first().cloned());
                    count += 1;
                }
                Poll::Ready(Some(_)) => {
                    context.waker().wake_by_ref();
                    break;
                }
                _ => break,
            }
        }
        if count == 0 {
            Poll::Pending
        } else {
            Poll::Ready(Ok(count))
        }
    }
}
