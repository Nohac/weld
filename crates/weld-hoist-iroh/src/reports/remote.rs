//! One bounded diagnostic RPC per independent QUIC stream.
use super::DiagnosticReports;
use crate::framing::{read_record, write_record};
use anyhow::{Context, Result};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    time::{Duration, Instant},
};
use weld_diagnostics::{Report, ReportBundle, SessionId};

pub(crate) const DEADLINE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CollectionStatus {
    Collected,
    Offline,
    Denied,
    Unavailable,
    TimedOut,
    Busy,
    Failed,
}
impl fmt::Display for CollectionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Collected => "matching peer report collected",
            Self::Offline => "peer is offline; reconnect to collect its report",
            Self::Denied => "peer denied session diagnostics access",
            Self::Unavailable => "peer report is unavailable or has expired",
            Self::TimedOut => "peer report request timed out; the peer may be unreachable or running an older build",
            Self::Busy => "diagnostic collection is busy; try again shortly",
            Self::Failed => "peer report request failed or returned invalid evidence",
        })
    }
}

#[derive(Serialize, Deserialize)]
pub struct DiagnosticCollection {
    pub bundle: ReportBundle,
    pub status: CollectionStatus,
}

type Reply = std::result::Result<Box<Report>, CollectionStatus>;

struct PendingRpc {
    send: SendStream,
    recv: RecvStream,
    completed: bool,
}
impl Drop for PendingRpc {
    fn drop(&mut self) {
        if !self.completed {
            let _ = self.send.reset(1_u32.into());
            let _ = self.recv.stop(1_u32.into());
        }
    }
}

pub(crate) async fn serve(reports: DiagnosticReports, connection: Connection) {
    let mut last_request: Option<Instant> = None;
    while let Ok((mut send, mut recv)) = connection.accept_bi().await {
        let result = tokio::time::timeout(DEADLINE, async {
            let session: SessionId = read_record(&mut recv).await?;
            let response: Reply =
                if last_request.is_some_and(|last| last.elapsed() < Duration::from_secs(1)) {
                    Err(CollectionStatus::Busy)
                } else {
                    last_request = Some(Instant::now());
                    reports
                        .local_for_peer(connection.remote_id(), session)
                        .map(Box::new)
                };
            write_record(&mut send, &response).await?;
            send.finish()?;
            Ok::<_, anyhow::Error>(())
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            let _ = send.reset(1_u32.into());
            let _ = recv.stop(1_u32.into());
        }
    }
}

impl DiagnosticReports {
    pub(crate) async fn collect(&self, session: SessionId) -> Result<DiagnosticCollection> {
        let mut bundle = self
            .get(session)
            .context("diagnostic session unavailable or evicted")?;
        let Some((owner, connection)) = self.route(session) else {
            return Ok(DiagnosticCollection {
                bundle,
                status: CollectionStatus::Offline,
            });
        };
        let request = async {
            let (send, recv) = connection.open_bi().await?;
            let mut rpc = PendingRpc {
                send,
                recv,
                completed: false,
            };
            write_record(&mut rpc.send, &session).await?;
            rpc.send.finish()?;
            let reply = read_record::<_, Reply>(&mut rpc.recv).await?;
            rpc.completed = true;
            Ok::<_, anyhow::Error>(reply)
        };
        let status = match tokio::time::timeout(DEADLINE, request).await {
            Ok(Ok(Ok(report)))
                if report.session == session && report.endpoint != bundle.local.endpoint =>
            {
                if self.exchange(owner, report.as_ref().clone()).is_ok() {
                    bundle.peer = Some(*report);
                    CollectionStatus::Collected
                } else {
                    CollectionStatus::Failed
                }
            }
            Ok(Ok(Err(status))) if status != CollectionStatus::Collected => status,
            Err(_) => CollectionStatus::TimedOut,
            _ => CollectionStatus::Failed,
        };
        Ok(DiagnosticCollection {
            bundle: self.get(session).unwrap_or(bundle),
            status,
        })
    }
}

#[cfg(test)]
mod tests;
