//! Explicit-peer admission and source/destination presentation exchange.
#[cfg(test)]
mod tests;
use super::{
    channel::{PacketReceive, PacketSend, receive_record, send_record},
    negotiation,
    pairing::{AuthenticatedPeer, ConnectionControl},
    server::DIRECT_SERVICE,
};
use anyhow::{Context, Result, ensure};
use async_channel::Receiver;
use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    task::JoinSet,
    time::{timeout, timeout_at},
};
use weld_client::SurfaceStreamMode;
use weld_hoist_protocol::{
    ProtocolRevision, SurfaceMode,
    bootstrap::{BootstrapAnswer, BootstrapOffer, PeerRole},
    presentation::ReceiverPreferences,
};
use weld_session::{Connector, Wake};

/// Selected delivery mode. Native delivery carries no codec requirement.
#[derive(Clone, Copy, Debug)]
pub struct Presentation {
    pub mode: SurfaceMode,
    pub stream_mode: SurfaceStreamMode,
}
/// Endpoint delivery support, already restricted by the connection's facilities.
pub struct Delivery {
    pub native_buffers: bool,
    pub encoded: ReceiverPreferences,
}
impl Delivery {
    pub fn validate(&self, mode: SurfaceMode) -> Result<()> {
        match mode {
            SurfaceMode::Native => {
                ensure!(self.native_buffers, "native buffer delivery unavailable")
            }
            SurfaceMode::EncodedOpaque(codec) => {
                negotiation::presentation(&self.encoded, codec)?;
            }
        }
        Ok(())
    }
}
/// Hoist attachment capabilities after an explicit admission and bootstrap.
/// Bindings perform descriptor/lane setup and retain their I/O executor.
pub trait Connection: Send + Sync + 'static {
    type Writer: PacketSend + 'static;
    type Reader: PacketReceive + 'static;
    type Source: ConnectionControl + Send + 'static;
    type Destination: ConnectionControl + Send + 'static;
    /// The binding can transfer native buffer descriptors and synchronization.
    fn native_buffers(&self) -> bool {
        false
    }
    fn identity(&self) -> Result<AuthenticatedPeer>;
    fn open(&self) -> impl Future<Output = Result<(Self::Writer, Self::Reader)>> + Send;
    fn accept(&self) -> impl Future<Output = Result<(Self::Writer, Self::Reader)>> + Send;
    fn source(
        self,
        lane: (Self::Writer, Self::Reader),
        presentation: Presentation,
        wake: Wake,
    ) -> impl Future<Output = Result<Self::Source>> + Send;
    fn destination(
        self,
        lane: (Self::Writer, Self::Reader),
        presentation: Presentation,
        wake: Wake,
    ) -> impl Future<Output = Result<Self::Destination>> + Send;
}

/// Only approved identities may receive the source's offer or application metadata.
pub trait Incoming: Send {
    type Connection: Connection;
    fn next(&mut self) -> impl Future<Output = Result<Self::Connection>> + Send;
}
struct Retained<P: ConnectionControl>(Option<P>);
impl<P: ConnectionControl> Drop for Retained<P> {
    fn drop(&mut self) {
        if let Some(peer) = &self.0 {
            peer.close();
        }
    }
}
impl<C: Connection> Incoming for Receiver<C> {
    type Connection = C;
    async fn next(&mut self) -> Result<C> {
        self.recv().await.context("source listener closed")
    }
}
pub async fn accept<I: Incoming>(
    mut incoming: I,
    approved: Vec<AuthenticatedPeer>,
    mode: SurfaceMode,
    deadline: Instant,
    wake: Wake,
) -> Result<<I::Connection as Connection>::Source> {
    ensure!(
        !approved.is_empty() && approved.len() <= 32,
        "source admission requires 1..32 approved peers"
    );
    let approved = Arc::new(approved);
    let mut candidates = JoinSet::new();
    let result = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline.into()) => break Err(anyhow::anyhow!("source admission timed out")),
            arrival = incoming.next(), if candidates.len() < 8 => {
                let connection = match arrival { Ok(connection) => connection, Err(error) => break Err(error) };
                let approved = approved.clone();
                let wake = wake.clone();
                candidates.spawn(async move {
                    timeout(Duration::from_secs(5), async move {
                        ensure!(approved.contains(&connection.identity()?), "peer is not an approved destination");
                        ensure!(mode != SurfaceMode::Native || connection.native_buffers(), "native buffer transfer unavailable on this connection");
                        let (mut writer, mut reader) = connection.open().await?;
                        let presentation = source_exchange(&mut writer, &mut reader, mode).await?;
                        connection.source((writer, reader), presentation, wake).await.map(|peer| Retained(Some(peer)))
                    }).await.context("source bootstrap timed out")?
                });
            },
            completed = candidates.join_next(), if !candidates.is_empty() => {
                match completed.context("candidate task disappeared")?.context("candidate task failed")? {
                    Ok(peer) => break Ok(peer),
                    Err(error) => tracing::debug!(%error, "source candidate rejected"),
                }
            },
        }
    };
    candidates.shutdown().await;
    result?.0.take().context("source result already claimed")
}

pub async fn connect<C>(
    connector: C,
    delivery: Delivery,
    deadline: Instant,
    wake: Wake,
) -> Result<<C::Connection as Connection>::Destination>
where
    C: Connector,
    C::Connection: Connection,
{
    timeout_at(deadline.into(), async {
        let connection = connector.connect(DIRECT_SERVICE).await?;
        let mut delivery = delivery;
        delivery.native_buffers &= connection.native_buffers();
        let (mut writer, mut reader) = connection.accept().await?;
        let presentation = destination_exchange(&mut writer, &mut reader, &delivery).await?;
        connection
            .destination((writer, reader), presentation, wake)
            .await
    })
    .await
    .context("destination bootstrap timed out")?
}
pub async fn source_exchange(
    writer: &mut impl PacketSend,
    reader: &mut impl PacketReceive,
    mode: SurfaceMode,
) -> Result<Presentation> {
    send_record(
        writer,
        &BootstrapOffer {
            revision: ProtocolRevision::CURRENT,
            role: PeerRole::Source,
            mode,
        },
    )
    .await?;
    let answer: BootstrapAnswer = receive_record(reader).await?;
    negotiation::answer(&answer)?;
    Ok(Presentation {
        mode,
        stream_mode: answer.stream_mode,
    })
}
pub async fn destination_exchange(
    writer: &mut impl PacketSend,
    reader: &mut impl PacketReceive,
    delivery: &Delivery,
) -> Result<Presentation> {
    let offer: BootstrapOffer = receive_record(reader).await?;
    let result = (|| {
        ProtocolRevision::CURRENT.ensure_compatible(offer.revision)?;
        ensure!(
            offer.role == PeerRole::Source,
            "bootstrap peer has the wrong role"
        );
        delivery.validate(offer.mode)
    })();
    send_record(
        writer,
        &BootstrapAnswer {
            revision: ProtocolRevision::CURRENT,
            role: PeerRole::Destination,
            rejection: result.as_ref().err().map(ToString::to_string),
            stream_mode: delivery.encoded.stream_mode,
        },
    )
    .await?;
    result?;
    Ok(Presentation {
        mode: offer.mode,
        stream_mode: delivery.encoded.stream_mode,
    })
}
