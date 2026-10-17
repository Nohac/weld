//! Shared diagnostic exchange policy over independent bounded packet lanes.

use super::{
    channel::{receive_record, send_record},
    services::ServiceTransport,
};
use anyhow::{Context, Result};
use async_io::Timer;
use futures_lite::future::race;
use std::{
    future::Future,
    time::{Duration, Instant},
};
use weld_diagnostics::{
    Report, SessionId,
    archive::ReportArchive,
    collection::{CollectionStatus, DiagnosticCollection, RequestBudget},
};

const DEADLINE: Duration = Duration::from_secs(3);
type Reply = Result<Box<Report>, CollectionStatus>;

/// Starts the diagnostic service for an admitted connection. The binding
/// supplies incoming packet lanes; shared code owns its request budget and loop.
pub fn listen<P, L, F, Fut>(
    tasks: &weld_session::SessionTasks,
    archive: ReportArchive<P>,
    owner: P,
    next: F,
) -> Result<()>
where
    P: Eq + Clone + Send + Sync + 'static,
    L: ServiceTransport + Send + 'static,
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<L>> + Send + 'static,
{
    tasks.spawn(
        "weld/diagnostic-peer/1",
        32,
        serve_streams(archive, owner, next),
    )
}
pub async fn serve_streams<P, L, F, Fut>(
    archive: ReportArchive<P>,
    owner: P,
    mut next: F,
) -> Result<()>
where
    P: Eq + Clone + Send + Sync,
    L: ServiceTransport,
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = Result<L>> + Send,
{
    let mut budget = RequestBudget::default();
    while let Ok(lane) = next().await {
        if let Err(error) = serve(&archive, &owner, &mut budget, lane).await {
            tracing::debug!(%error, "diagnostic lane ended");
        }
    }
    Ok(())
}

pub async fn serve<P: Eq + Clone>(
    archive: &ReportArchive<P>,
    owner: &P,
    budget: &mut RequestBudget,
    mut lane: impl ServiceTransport,
) -> Result<()> {
    race(
        async {
            let session = receive_record::<SessionId>(&mut lane).await?;
            let reply: Reply = if budget.admit(Instant::now()) {
                archive.local_for_peer(owner, session).map(Box::new)
            } else {
                Err(CollectionStatus::Busy)
            };
            send_record(&mut lane, &reply).await?;
            lane.finish().await
        },
        async {
            Timer::after(DEADLINE).await;
            anyhow::bail!("diagnostic request timed out")
        },
    )
    .await
}

/// The deadline includes opening the lane. Failed collection preserves cached evidence.
pub async fn collect<P: Eq + Clone, L: ServiceTransport>(
    archive: &ReportArchive<P>,
    owner: &P,
    session: SessionId,
    open: impl Future<Output = Result<L>>,
) -> Result<DiagnosticCollection> {
    let bundle = archive
        .get(session)
        .context("diagnostic session unavailable or evicted")?;
    let exchange = async {
        let mut lane = open.await?;
        send_record(&mut lane, &session).await?;
        receive_record::<Reply>(&mut lane).await
    };
    let status = match race(async { Some(exchange.await) }, async {
        Timer::after(DEADLINE).await;
        None
    })
    .await
    {
        Some(Ok(Ok(report)))
            if report.session == session && report.endpoint != bundle.local.endpoint =>
        {
            if archive.cache_peer(owner, *report).is_ok() {
                CollectionStatus::Collected
            } else {
                CollectionStatus::Failed
            }
        }
        Some(Ok(Err(status))) if status != CollectionStatus::Collected => status,
        None => CollectionStatus::TimedOut,
        _ => CollectionStatus::Failed,
    };
    Ok(DiagnosticCollection {
        bundle: archive.get(session).unwrap_or(bundle),
        status,
    })
}
