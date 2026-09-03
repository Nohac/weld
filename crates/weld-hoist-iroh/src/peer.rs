//! Nonblocking compositor-facing peer handles and async stream drivers.

use std::{
    collections::VecDeque,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, ensure};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::sync::mpsc;
use weld_core::host::ClientRuntimeNotifier;
use weld_hoist_core::{HoistPortError, HoistPortResult};
use weld_hoist_encoded::{
    EncodedDestinationTransport, EncodedSourceTransport, SourceTransportPacket,
};
use weld_hoist_protocol::{DestinationEnvelope, SourceEnvelope};
use weld_media::EncodedAccessUnit;
use weld_media::VideoCodec;

use crate::{
    IrohPeerIdentity,
    framing::{read_media, read_record, write_media, write_record},
    host::HostLifetime,
};

const QUEUE_CAPACITY: usize = 256;
const MEDIA_STREAM_MAGIC: [u8; 8] = *b"weldmed1";

struct PeerState<T> {
    incoming: IncomingQueue<T>,
    connection: Connection,
    notifier: ClientRuntimeNotifier,
}

struct IncomingQueue<T> {
    available: AtomicBool,
    values: Mutex<VecDeque<T>>,
}

impl<T> IncomingQueue<T> {
    fn new() -> Self {
        Self {
            available: AtomicBool::new(true),
            values: Mutex::new(VecDeque::new()),
        }
    }

    fn push(&self, value: T) -> Result<()> {
        let mut values = self
            .values
            .lock()
            .map_err(|_| anyhow::anyhow!("Iroh peer input queue lock is poisoned"))?;
        ensure!(
            values.len() < QUEUE_CAPACITY,
            "Iroh peer input queue is full"
        );
        values.push_back(value);
        Ok(())
    }

    fn drain(&self) -> HoistPortResult<Vec<T>> {
        let mut values = self
            .values
            .lock()
            .map_err(|_| peer_error("Iroh peer input queue lock is poisoned"))?;
        if values.is_empty() && !self.is_available() {
            return Err(peer_error("Iroh peer is unavailable"));
        }
        Ok(values.drain(..).collect())
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }

    fn fail(&self) -> bool {
        self.available.swap(false, Ordering::AcqRel)
    }
}

impl<T> PeerState<T> {
    fn new(connection: Connection, notifier: ClientRuntimeNotifier) -> Self {
        Self {
            incoming: IncomingQueue::new(),
            connection,
            notifier,
        }
    }

    fn push(&self, value: T) -> Result<()> {
        self.incoming.push(value)?;
        self.notifier
            .notify()
            .context("could not wake Weld for Iroh input")
    }

    fn drain(&self) -> HoistPortResult<Vec<T>> {
        self.incoming.drain()
    }

    fn is_available(&self) -> bool {
        self.incoming.is_available()
    }

    fn fail(&self) {
        if self.incoming.fail() {
            self.connection.close(1_u32.into(), b"weld peer closed");
            let _ = self.notifier.notify();
        }
    }
}

/// Source-facing half of one authenticated Iroh peer connection.
#[derive(Clone)]
pub struct IrohSourcePeer {
    state: Arc<PeerState<DestinationEnvelope>>,
    control: mpsc::Sender<SourceEnvelope<weld_hoist_protocol::EncodedBuffer>>,
    media: mpsc::Sender<weld_hoist_protocol::MediaEnvelope<EncodedAccessUnit>>,
    identity: IrohPeerIdentity,
    codec: VideoCodec,
    _host: Arc<HostLifetime>,
}

impl IrohSourcePeer {
    pub fn identity(&self) -> &IrohPeerIdentity {
        &self.identity
    }

    pub fn is_available(&self) -> bool {
        self.state.is_available()
    }

    pub const fn codec(&self) -> VideoCodec {
        self.codec
    }
}

impl EncodedSourceTransport for IrohSourcePeer {
    fn send(&self, packet: SourceTransportPacket) -> HoistPortResult<()> {
        match packet {
            SourceTransportPacket::Control(packet) => {
                self.control.try_send(packet).map_err(|error| {
                    self.state.fail();
                    peer_error(format!("could not queue Iroh source control: {error}"))
                })
            }
            SourceTransportPacket::Media(packet) => self.media.try_send(packet).map_err(|error| {
                self.state.fail();
                peer_error(format!("could not queue Iroh source media: {error}"))
            }),
        }
    }

    fn drain(&self) -> HoistPortResult<Vec<DestinationEnvelope>> {
        self.state.drain()
    }

    fn disconnect(&self) {
        self.state.fail();
    }
}

/// Destination-facing half of one authenticated Iroh peer connection.
#[derive(Clone)]
pub struct IrohDestinationPeer {
    state: Arc<PeerState<SourceTransportPacket>>,
    control: mpsc::Sender<DestinationEnvelope>,
    identity: IrohPeerIdentity,
    codec: VideoCodec,
    _host: Arc<HostLifetime>,
}

impl IrohDestinationPeer {
    pub fn identity(&self) -> &IrohPeerIdentity {
        &self.identity
    }

    pub fn is_available(&self) -> bool {
        self.state.is_available()
    }

    pub const fn codec(&self) -> VideoCodec {
        self.codec
    }
}

impl EncodedDestinationTransport for IrohDestinationPeer {
    fn send(&self, packet: DestinationEnvelope) -> HoistPortResult<()> {
        self.control.try_send(packet).map_err(|error| {
            self.state.fail();
            peer_error(format!("could not queue Iroh destination packet: {error}"))
        })
    }

    fn drain(&self) -> HoistPortResult<Vec<SourceTransportPacket>> {
        self.state.drain()
    }

    fn disconnect(&self) {
        self.state.fail();
    }
}

pub(crate) fn spawn_source_peer(
    host: Arc<HostLifetime>,
    connection: Connection,
    control_send: SendStream,
    control_recv: RecvStream,
    media_send: SendStream,
    notifier: ClientRuntimeNotifier,
    codec: VideoCodec,
) -> IrohSourcePeer {
    let identity = IrohPeerIdentity(connection.remote_id().to_string());
    let state = Arc::new(PeerState::new(connection.clone(), notifier));
    let (control_tx, control_rx) = mpsc::channel(QUEUE_CAPACITY);
    let (media_tx, media_rx) = mpsc::channel(QUEUE_CAPACITY);
    tokio::spawn(run_source_peer(
        state.clone(),
        control_send,
        control_recv,
        control_rx,
        media_send,
        media_rx,
    ));
    IrohSourcePeer {
        state,
        control: control_tx,
        media: media_tx,
        identity,
        codec,
        _host: host,
    }
}

pub(crate) fn spawn_destination_peer(
    host: Arc<HostLifetime>,
    connection: Connection,
    control_send: SendStream,
    control_recv: RecvStream,
    media_recv: RecvStream,
    notifier: ClientRuntimeNotifier,
    codec: VideoCodec,
) -> IrohDestinationPeer {
    let identity = IrohPeerIdentity(connection.remote_id().to_string());
    let state = Arc::new(PeerState::new(connection.clone(), notifier));
    let (control_tx, control_rx) = mpsc::channel(QUEUE_CAPACITY);
    tokio::spawn(run_destination_peer(
        state.clone(),
        control_send,
        control_recv,
        control_rx,
        media_recv,
    ));
    IrohDestinationPeer {
        state,
        control: control_tx,
        identity,
        codec,
        _host: host,
    }
}

async fn run_source_peer(
    state: Arc<PeerState<DestinationEnvelope>>,
    mut control_send: SendStream,
    control_recv: RecvStream,
    mut outgoing_control: mpsc::Receiver<SourceEnvelope<weld_hoist_protocol::EncodedBuffer>>,
    mut media_send: SendStream,
    mut outgoing_media: mpsc::Receiver<weld_hoist_protocol::MediaEnvelope<EncodedAccessUnit>>,
) {
    let control_writer = async move {
        while let Some(packet) = outgoing_control.recv().await {
            write_record(&mut control_send, &packet).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let control_reader = read_destination_control(state.clone(), control_recv);
    let media_writer = async move {
        media_send
            .write_all(&MEDIA_STREAM_MAGIC)
            .await
            .context("could not initialize Iroh media stream")?;
        while let Some(packet) = outgoing_media.recv().await {
            write_media(&mut media_send, packet).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    if let Err(error) = tokio::try_join!(control_writer, control_reader, media_writer) {
        tracing::warn!(%error, "Iroh source peer stopped");
    }
    state.fail();
}

async fn run_destination_peer(
    state: Arc<PeerState<SourceTransportPacket>>,
    mut control_send: SendStream,
    control_recv: RecvStream,
    mut outgoing_control: mpsc::Receiver<DestinationEnvelope>,
    media_recv: RecvStream,
) {
    let control_writer = async move {
        while let Some(packet) = outgoing_control.recv().await {
            write_record(&mut control_send, &packet).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let control_reader = read_source_control(state.clone(), control_recv);
    let media = read_source_media(state.clone(), media_recv);
    if let Err(error) = tokio::try_join!(control_writer, control_reader, media) {
        tracing::warn!(%error, "Iroh destination peer stopped");
    }
    state.fail();
}

async fn read_destination_control(
    state: Arc<PeerState<DestinationEnvelope>>,
    mut stream: RecvStream,
) -> anyhow::Result<()> {
    loop {
        state.push(read_record::<_, DestinationEnvelope>(&mut stream).await?)?;
    }
}

async fn read_source_control(
    state: Arc<PeerState<SourceTransportPacket>>,
    mut stream: RecvStream,
) -> anyhow::Result<()> {
    loop {
        state.push(SourceTransportPacket::Control(
            read_record::<_, SourceEnvelope<weld_hoist_protocol::EncodedBuffer>>(&mut stream)
                .await?,
        ))?;
    }
}

async fn read_source_media(
    state: Arc<PeerState<SourceTransportPacket>>,
    mut stream: RecvStream,
) -> anyhow::Result<()> {
    let mut magic = [0; MEDIA_STREAM_MAGIC.len()];
    stream
        .read_exact(&mut magic)
        .await
        .context("could not initialize Iroh media stream")?;
    ensure!(magic == MEDIA_STREAM_MAGIC, "invalid Iroh media stream");
    loop {
        state.push(SourceTransportPacket::Media(read_media(&mut stream).await?))?;
    }
}

fn peer_error(error: impl fmt::Display) -> HoistPortError {
    Box::new(PeerError(error.to_string()))
}

#[derive(Debug)]
struct PeerError(String);

impl fmt::Display for PeerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PeerError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_queue_drains_buffered_records_before_reporting_failure() {
        let incoming = IncomingQueue::new();
        incoming.push(1_u8).expect("available push");
        assert_eq!(incoming.drain().expect("available drain"), vec![1]);

        incoming.push(2).expect("buffered push");
        assert!(incoming.fail());
        assert!(!incoming.is_available());
        assert_eq!(incoming.drain().expect("failed buffered drain"), vec![2]);
        assert!(incoming.drain().is_err());
    }
}
