//! One packet path over ADB-forwarded TCP. Not a production transport.
use crate::wire;
use iroh::{
    EndpointId,
    endpoint::transports::{CustomEndpoint, CustomSender, CustomTransport, RecvInfo, Transmit},
};
use iroh_base::CustomAddr;
use n0_watcher::{Direct, Watchable};
use serde::Serialize;
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{io::AsyncWriteExt, net::TcpStream, sync::mpsc, task::JoinHandle};

// Private experiment identifier, not upstream's reserved in-memory test id 0x20.
pub const TRANSPORT_ID: u64 = 0x77656c64616462;
// Two 96 Mbps / 90 Hz frames are roughly 224 initial-MTU datagrams.
// Unlike the receive queue, this queue cannot apply upstream backpressure.
pub const SEND_CAPACITY: usize = 256;

pub fn address(id: EndpointId) -> CustomAddr {
    CustomAddr::from_parts(TRANSPORT_ID, id.as_bytes())
}

#[derive(Debug, Default)]
pub struct Counters {
    sent: AtomicU64,
    received: AtomicU64,
    dropped: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Snapshot {
    pub sent_packets: u64,
    pub received_packets: u64,
    pub adapter_dropped_packets: u64,
}

impl Counters {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            sent_packets: self.sent.load(Ordering::Relaxed),
            received_packets: self.received.load(Ordering::Relaxed),
            adapter_dropped_packets: self.dropped.load(Ordering::Relaxed),
        }
    }
}

pub struct Driver(pub JoinHandle<io::Result<()>>);
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug)]
pub struct Factory {
    receive: Mutex<Option<mpsc::Receiver<Vec<u8>>>>,
    sender: Arc<Sender>,
    local: CustomAddr,
}

#[derive(Debug)]
struct Sender {
    send: mpsc::Sender<Vec<u8>>,
    remote: CustomAddr,
    counters: Arc<Counters>,
}

impl Sender {
    fn enqueue(&self, packet: &[u8]) -> io::Result<()> {
        if packet.is_empty() || packet.len() > wire::MAX_PACKET {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid QUIC packet length",
            ));
        }
        match self.send.try_send(packet.to_vec()) {
            Ok(()) => {
                self.counters.sent.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            // Iroh 1.1.0 silently blackholes Pending from custom poll_send.
            // Model bounded queue overflow as explicit packet loss, not backpressure.
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "tunnel writer closed",
            )),
        }
    }
}

impl CustomSender for Sender {
    fn is_valid_send_addr(&self, addr: &CustomAddr) -> bool {
        addr == &self.remote
    }
    fn poll_send(
        &self,
        _: &mut Context<'_>,
        dst: &CustomAddr,
        _: Option<&CustomAddr>,
        tx: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        if dst != &self.remote || tx.segment_size.is_some_and(|size| size < tx.contents.len()) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unexpected destination or GSO",
            )));
        }
        Poll::Ready(self.enqueue(tx.contents))
    }
}

#[derive(Debug)]
struct Endpoint {
    receive: mpsc::Receiver<Vec<u8>>,
    sender: Arc<Sender>,
    local: Watchable<Vec<CustomAddr>>,
}

impl CustomTransport for Factory {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
        let receive = self
            .receive
            .lock()
            .map_err(|_| io::Error::other("receive lock poisoned"))?
            .take()
            .ok_or_else(|| io::Error::other("tunnel already bound"))?;
        Ok(Box::new(Endpoint {
            receive,
            sender: self.sender.clone(),
            local: Watchable::new(vec![self.local.clone()]),
        }))
    }
}

impl CustomEndpoint for Endpoint {
    fn watch_local_addrs(&self) -> Direct<Vec<CustomAddr>> {
        self.local.watch()
    }
    fn create_sender(&self) -> Arc<dyn CustomSender> {
        self.sender.clone()
    }
    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        metas: &mut [noq_udp::RecvMeta],
        infos: &mut [RecvInfo],
    ) -> Poll<io::Result<usize>> {
        let (Some(buf), Some(meta), Some(info)) =
            (bufs.first_mut(), metas.first_mut(), infos.first_mut())
        else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty receive slots",
            )));
        };
        match self.receive.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "tunnel reader closed",
            ))),
            Poll::Ready(Some(packet)) => {
                if packet.len() > buf.len() {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "receive buffer too small",
                    )));
                }
                buf[..packet.len()].copy_from_slice(&packet);
                *meta = noq_udp::RecvMeta::default();
                meta.len = packet.len();
                meta.stride = packet.len();
                *info = RecvInfo::new(
                    self.sender.remote.clone(),
                    self.local.get().first().cloned(),
                );
                Poll::Ready(Ok(1))
            }
        }
    }
}

pub fn open(
    stream: TcpStream,
    local: EndpointId,
    remote: EndpointId,
) -> io::Result<(Arc<Factory>, Driver, Arc<Counters>)> {
    stream.set_nodelay(true)?;
    let (mut reader, mut writer) = stream.into_split();
    let (outgoing, mut output) = mpsc::channel::<Vec<u8>>(SEND_CAPACITY);
    let (incoming, input) = mpsc::channel(wire::CAPACITY);
    let counters = Arc::new(Counters::default());
    let observations = counters.clone();
    let driver = Driver(tokio::spawn(async move {
        let read = async {
            while let Some(packet) = wire::read(&mut reader).await? {
                observations.received.fetch_add(1, Ordering::Relaxed);
                incoming
                    .send(packet)
                    .await
                    .map_err(|_| io::Error::other("receiver closed"))?;
            }
            Err::<(), io::Error>(io::Error::new(io::ErrorKind::UnexpectedEof, "tunnel EOF"))
        };
        let write = async {
            while let Some(packet) = output.recv().await {
                wire::write(&mut writer, &packet).await?;
            }
            writer.shutdown().await
        };
        // Any failure cancels the other half, dropping both channel endpoints.
        tokio::try_join!(read, write).map(|_| ())
    }));
    let factory = Arc::new(Factory {
        receive: Mutex::new(Some(input)),
        sender: Arc::new(Sender {
            send: outgoing,
            remote: address(remote),
            counters: counters.clone(),
        }),
        local: address(local),
    });
    Ok((factory, driver, counters))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn socket_eof_closes_both_packet_queues() -> io::Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let peer = TcpStream::connect(listener.local_addr()?).await?;
        let (socket, _) = listener.accept().await?;
        let id = iroh::SecretKey::generate().public();
        let (factory, mut driver, _) = open(socket, id, id)?;
        let mut receiver = factory
            .receive
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?
            .take()
            .ok_or_else(|| io::Error::other("missing receiver"))?;
        drop(peer);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), &mut driver.0)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
        assert!(result.is_err());
        assert!(receiver.recv().await.is_none());
        assert!(factory.sender.enqueue(b"closed").is_err());
        Ok(())
    }

    #[test]
    fn full_queue_is_counted_as_loss_and_closed_queue_fails() {
        let (send, receive) = mpsc::channel(1);
        let counters = Arc::new(Counters::default());
        let sender = Sender {
            send,
            remote: address(iroh::SecretKey::generate().public()),
            counters: counters.clone(),
        };
        sender.enqueue(b"first").expect("first");
        sender.enqueue(b"dropped").expect("loss");
        assert_eq!(counters.snapshot().adapter_dropped_packets, 1);
        drop(receive);
        assert!(sender.enqueue(b"closed").is_err());
    }
}
