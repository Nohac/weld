//! Nonblocking compositor-facing peer handles and async stream drivers.

use std::{fmt, sync::Arc, time::Instant};

use anyhow::{Context, Result, ensure};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    sync::mpsc,
};
use weld_core::host::ClientRuntimeNotifier;
use weld_hoist_core::{HoistPortError, HoistPortResult};
use weld_hoist_encoded::{
    EncodedDestinationTransport, EncodedSourceTransport, SourceTransportPacket, TransportSnapshot,
};
use weld_hoist_protocol::{DestinationEnvelope, SourceEnvelope};
use weld_media::VideoCodec;

use crate::{
    IrohPeerIdentity,
    diagnostics::PathMonitor,
    framing::{read_media, read_record, write_record},
    host::HostLifetime,
    inbox::IncomingQueue,
    input_outbox::InputOutbox,
    media_queue::{MediaSender, QueuedMedia, write_media_queue},
};

pub(super) const QUEUE_CAPACITY: usize = 256;
const MEDIA_STREAM_MAGIC: [u8; 8] = *b"weldmed1";

struct PeerState<T> {
    incoming: IncomingQueue<T>,
    connection: Connection,
}

impl<T> PeerState<T> {
    fn new(connection: Connection, notifier: ClientRuntimeNotifier) -> Self {
        Self {
            incoming: IncomingQueue::new(notifier),
            connection,
        }
    }

    fn drain(&self) -> HoistPortResult<Vec<T>> {
        self.incoming.drain().map_err(peer_error)
    }

    fn is_available(&self) -> bool {
        self.incoming.is_available()
    }

    fn fail(&self) {
        if self.incoming.fail() {
            self.connection.close(1_u32.into(), b"weld peer closed");
        }
    }
}

/// Source-facing half of one authenticated Iroh peer connection.
#[derive(Clone)]
pub struct IrohSourcePeer {
    state: Arc<PeerState<DestinationEnvelope>>,
    control: mpsc::Sender<SourceEnvelope<weld_hoist_protocol::EncodedBuffer>>,
    media: MediaSender,
    path: PathMonitor,
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

    fn observations(&self, now: Instant) -> Option<TransportSnapshot> {
        Some(TransportSnapshot {
            observed_at: now,
            media: self.media.snapshot(now)?,
            path: if self.state.is_available() {
                self.path.snapshot()
            } else {
                None
            },
        })
    }
}

/// Destination-facing half of one authenticated Iroh peer connection.
#[derive(Clone)]
pub struct IrohDestinationPeer {
    state: Arc<PeerState<SourceTransportPacket>>,
    control: Arc<InputOutbox>,
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
        self.control.push(packet).map_err(|error| {
            self.state.fail();
            peer_error(format!("could not queue Iroh destination packet: {error}"))
        })
    }

    fn drain(&self) -> HoistPortResult<Vec<SourceTransportPacket>> {
        self.state.drain()
    }

    fn disconnect(&self) {
        self.control.close();
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
    let path = crate::diagnostics::observe(&connection);
    let identity = IrohPeerIdentity(connection.remote_id().to_string());
    let state = Arc::new(PeerState::new(connection.clone(), notifier));
    let (control_tx, control_rx) = mpsc::channel(QUEUE_CAPACITY);
    let (media_tx, media_rx) = MediaSender::channel(QUEUE_CAPACITY);
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
        path,
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
    crate::diagnostics::observe(&connection);
    let identity = IrohPeerIdentity(connection.remote_id().to_string());
    let state = Arc::new(PeerState::new(connection.clone(), notifier));
    let control = Arc::new(InputOutbox::default());
    tokio::spawn(run_destination_peer(
        state.clone(),
        control_send,
        control_recv,
        control.clone(),
        media_recv,
    ));
    IrohDestinationPeer {
        state,
        control,
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
    mut outgoing_media: mpsc::Receiver<QueuedMedia>,
) {
    let control_writer = async move {
        while let Some(packet) = outgoing_control.recv().await {
            write_record(&mut control_send, &packet).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let control_reader = read_destination_control(&state.incoming, control_recv);
    let media_writer = async move {
        media_send
            .write_all(&MEDIA_STREAM_MAGIC)
            .await
            .context("could not initialize Iroh media stream")?;
        write_media_queue(&mut media_send, &mut outgoing_media).await?;
        Ok::<(), anyhow::Error>(())
    };
    tokio::select! {
        result = async { tokio::try_join!(control_writer, control_reader, media_writer) } => {
            if let Err(error) = result {
                tracing::warn!(error = %format_args!("{error:#}"), "Iroh source peer stopped");
            }
        }
        reason = state.connection.closed() => {
            tracing::debug!(%reason, "Iroh source connection closed");
        }
    }
    state.fail();
}

async fn run_destination_peer(
    state: Arc<PeerState<SourceTransportPacket>>,
    mut control_send: SendStream,
    control_recv: RecvStream,
    outgoing_control: Arc<InputOutbox>,
    media_recv: RecvStream,
) {
    let control_writer = write_destination_control(&outgoing_control, &mut control_send);
    let control_reader = read_source_control(&state.incoming, control_recv);
    let media = read_source_media(&state.incoming, media_recv);
    tokio::select! {
        result = async { tokio::try_join!(control_writer, control_reader, media) } => {
            if let Err(error) = result {
                tracing::warn!(error = %format_args!("{error:#}"), "Iroh destination peer stopped");
            }
        }
        reason = state.connection.closed() => {
            tracing::debug!(%reason, "Iroh destination connection closed");
        }
    }
    outgoing_control.close();
    state.fail();
}

async fn read_destination_control<R: AsyncRead + Unpin>(
    incoming: &IncomingQueue<DestinationEnvelope>,
    mut stream: R,
) -> anyhow::Result<()> {
    loop {
        incoming
            .push(read_record::<_, DestinationEnvelope>(&mut stream).await?)
            .await?;
    }
}

pub(super) async fn write_destination_control<W: AsyncWrite + Unpin>(
    outgoing: &InputOutbox,
    writer: &mut W,
) -> Result<()> {
    loop {
        let packet = outgoing.recv().await?;
        write_record(writer, &packet).await?;
    }
}

async fn read_source_control<R: AsyncRead + Unpin>(
    incoming: &IncomingQueue<SourceTransportPacket>,
    mut stream: R,
) -> anyhow::Result<()> {
    loop {
        incoming
            .push(SourceTransportPacket::Control(
                read_record::<_, SourceEnvelope<weld_hoist_protocol::EncodedBuffer>>(&mut stream)
                    .await?,
            ))
            .await?;
    }
}

async fn read_source_media<R: AsyncRead + Unpin>(
    incoming: &IncomingQueue<SourceTransportPacket>,
    mut stream: R,
) -> anyhow::Result<()> {
    let mut magic = [0; MEDIA_STREAM_MAGIC.len()];
    stream
        .read_exact(&mut magic)
        .await
        .context("could not initialize Iroh media stream")?;
    ensure!(magic == MEDIA_STREAM_MAGIC, "invalid Iroh media stream");
    loop {
        incoming
            .push(SourceTransportPacket::Media(read_media(&mut stream).await?))
            .await?;
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
    use futures_lite::future::poll_once;
    use weld_core::host::client_runtime_notifier;
    use weld_hoist_protocol::{DestinationMessage, HoistSessionId};

    use super::*;

    #[tokio::test]
    async fn saturated_stream_reader_does_not_block_opposite_control_writer() {
        let (notifier, _wake) = client_runtime_notifier().expect("notifier");
        let inbox = IncomingQueue::new(notifier);
        let (mut input, reader) = tokio::io::duplex(8192);
        for sequence in 0..QUEUE_CAPACITY + 1 {
            write_record(
                &mut input,
                &DestinationEnvelope {
                    session: HoistSessionId::new(sequence as u64),
                    message: DestinationMessage::Reclaim,
                },
            )
            .await
            .expect("input record");
        }
        let mut reading = Box::pin(read_destination_control(&inbox, reader));
        // Tokio IO can cooperatively yield before capacity is reached.
        for _ in 0..8 {
            assert!(poll_once(reading.as_mut()).await.is_none());
            tokio::task::yield_now().await;
        }
        let outgoing = InputOutbox::default();
        outgoing
            .push(DestinationEnvelope {
                session: HoistSessionId::new(999),
                message: DestinationMessage::Reclaim,
            })
            .expect("opposite record");
        let (mut writer, mut output) = tokio::io::duplex(64);
        let mut writing = Box::pin(write_destination_control(&outgoing, &mut writer));
        assert!(poll_once(writing.as_mut()).await.is_none());
        let packet: DestinationEnvelope =
            read_record(&mut output).await.expect("opposite progress");
        assert_eq!(packet.session, HoistSessionId::new(999));
        let batch = inbox.drain().expect("full batch");
        assert_eq!(batch.len(), QUEUE_CAPACITY);
        for (sequence, packet) in batch.into_iter().enumerate() {
            assert_eq!(packet.session, HoistSessionId::new(sequence as u64));
        }
        assert!(poll_once(reading.as_mut()).await.is_none());
        assert_eq!(
            inbox.drain().expect("last record")[0].session,
            HoistSessionId::new(QUEUE_CAPACITY as u64)
        );
    }
}
