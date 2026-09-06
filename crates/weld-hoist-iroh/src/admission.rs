//! Per-session peer approval and bounded bootstrap, before any Weld application data.

use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use iroh::{
    Endpoint, EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
};
use iroh_tickets::endpoint::EndpointTicket;
use serde::{Deserialize, Serialize};
use tokio::{
    task::JoinSet,
    time::{Instant, timeout, timeout_at},
};
use weld_hoist_protocol::{ProtocolRevision, SurfaceMode};
use weld_media::VideoCodec;

use crate::framing::{read_record, write_record};

pub(crate) const WELD_ALPN: &[u8] = b"weld/hoist/1";
const MAX_CANDIDATES: usize = 8;
pub(crate) const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);

/// An established attempt remains close-on-drop until transferred into a live peer.
pub(crate) struct PendingConnection {
    pub connection: Connection,
    armed: bool,
}

impl PendingConnection {
    fn new(connection: Connection) -> Self {
        Self {
            connection,
            armed: true,
        }
    }

    pub fn hand_off(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingConnection {
    fn drop(&mut self) {
        if self.armed {
            self.connection
                .close(1_u32.into(), b"weld bootstrap not admitted");
        }
    }
}

pub(crate) struct SourceBootstrap {
    pub pending: PendingConnection,
    pub send: SendStream,
    pub recv: RecvStream,
    pub media: SendStream,
}

pub(crate) struct DestinationBootstrap {
    pub pending: PendingConnection,
    pub send: SendStream,
    pub recv: RecvStream,
    pub media: RecvStream,
    pub codec: VideoCodec,
}

pub(crate) async fn accept_source(
    endpoint: &Endpoint,
    expected: EndpointId,
    codec: VideoCodec,
    deadline: Instant,
    attempt_timeout: Duration,
) -> Result<SourceBootstrap> {
    let mut candidates = JoinSet::new();
    let mut rejected = 0_u64;
    let result = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                break Err(anyhow::anyhow!("timed out waiting for the approved Iroh peer ({rejected} attempts rejected)"));
            }
            incoming = endpoint.accept(), if candidates.len() < MAX_CANDIDATES => {
                let Some(incoming) = incoming else {
                    break Err(anyhow::anyhow!("Iroh endpoint closed before peer admission"));
                };
                candidates.spawn(async move {
                    timeout(attempt_timeout, async move {
                        let pending = PendingConnection::new(incoming.await.context("could not authenticate Iroh peer")?);
                        ensure!(pending.connection.remote_id() == expected, "Iroh peer is not the approved destination");
                        // No Weld offer or metadata may precede this identity check.
                        let (mut send, mut recv) = pending.connection.open_bi().await?;
                        write_record(&mut send, &BootstrapOffer {
                            revision: ProtocolRevision::CURRENT,
                            role: PeerRole::Source,
                            mode: SurfaceMode::EncodedOpaque(codec),
                        }).await?;
                        let answer: BootstrapAnswer = read_record(&mut recv).await?;
                        ProtocolRevision::CURRENT.ensure_compatible(answer.revision)?;
                        ensure!(answer.role == PeerRole::Destination, "Iroh bootstrap peer returned the wrong role");
                        if let Some(rejection) = answer.rejection {
                            bail!("Iroh destination rejected the session: {rejection}");
                        }
                        let media = pending.connection.open_uni().await?;
                        Ok(SourceBootstrap { pending, send, recv, media })
                    }).await.context("Iroh handshake/bootstrap attempt timed out")?
                });
            }
            result = candidates.join_next(), if !candidates.is_empty() => {
                let result = result.context("Iroh candidate task disappeared")
                    .and_then(|task| task.context("Iroh candidate task failed"))
                    .and_then(|bootstrap| bootstrap);
                match result {
                    Ok(bootstrap) => break Ok(bootstrap),
                    Err(error) => {
                        rejected = rejected.saturating_add(1);
                        // Peer-controlled failures must not generate unbounded logs.
                        if rejected <= 3 {
                            tracing::warn!(attempt = rejected, error = %format_args!("{error:#}"), "Iroh candidate rejected; continuing to wait for the approved peer");
                        }
                    }
                }
            }
        }
    };
    // Abort/drain every loser; completed results still hold armed close guards.
    candidates.shutdown().await;
    if rejected > 3 {
        tracing::warn!(
            rejected,
            "Iroh admission rejection summary (additional details suppressed)"
        );
    }
    result
}

pub(crate) async fn connect_destination(
    endpoint: &Endpoint,
    ticket: EndpointTicket,
    supported_codecs: &[VideoCodec],
    deadline: Instant,
) -> Result<DestinationBootstrap> {
    timeout_at(deadline, async {
        let pending = PendingConnection::new(
            endpoint
                .connect(ticket.endpoint_addr().clone(), WELD_ALPN)
                .await
                .context("could not connect to the approved Iroh source")?,
        );
        let (mut send, mut recv) = pending.connection.accept_bi().await?;
        let offer: BootstrapOffer = read_record(&mut recv).await?;
        let rejection = validate_offer(&offer, supported_codecs)
            .err()
            .map(|error| error.to_string());
        write_record(
            &mut send,
            &BootstrapAnswer {
                revision: ProtocolRevision::CURRENT,
                role: PeerRole::Destination,
                rejection: rejection.clone(),
            },
        )
        .await?;
        if let Some(rejection) = rejection {
            bail!("rejected Iroh source: {rejection}");
        }
        let SurfaceMode::EncodedOpaque(codec) = offer.mode else {
            bail!("accepted Iroh offer did not select an encoded codec");
        };
        let media = pending.connection.accept_uni().await?;
        Ok(DestinationBootstrap {
            pending,
            send,
            recv,
            media,
            codec,
        })
    })
    .await
    .context("Iroh destination connection/bootstrap timed out")?
}

fn validate_offer(offer: &BootstrapOffer, supported_codecs: &[VideoCodec]) -> Result<()> {
    ProtocolRevision::CURRENT.ensure_compatible(offer.revision)?;
    ensure!(
        offer.role == PeerRole::Source,
        "Iroh bootstrap peer has the wrong role"
    );
    let SurfaceMode::EncodedOpaque(codec) = offer.mode else {
        bail!("Iroh source selected an unsupported surface mode");
    };
    ensure!(
        supported_codecs.contains(&codec),
        "Iroh source selected an unsupported codec"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum PeerRole {
    Source,
    Destination,
}

#[derive(Debug, Deserialize, Serialize)]
struct BootstrapOffer {
    revision: ProtocolRevision,
    role: PeerRole,
    mode: SurfaceMode,
}

#[derive(Debug, Deserialize, Serialize)]
struct BootstrapAnswer {
    revision: ProtocolRevision,
    role: PeerRole,
    rejection: Option<String>,
}

#[cfg(test)]
mod tests;
