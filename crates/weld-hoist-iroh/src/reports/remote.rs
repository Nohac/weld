//! Independent diagnostic stream admission and authenticated peer routing.
use super::{CollectionStatus, DiagnosticCollection, DiagnosticReports};
use crate::channel::PacketLane;
use anyhow::{Context, Result};
use iroh::endpoint::Connection;
use weld_diagnostics::{SessionId, collection::RequestBudget};
use weld_hoist_core::session::diagnostics;

pub(crate) async fn serve(reports: DiagnosticReports, connection: Connection) {
    let mut budget = RequestBudget::default();
    while let Ok((send, recv)) = connection.accept_bi().await {
        if let Err(error) = diagnostics::serve(
            &reports.archive,
            &connection.remote_id(),
            &mut budget,
            PacketLane::new(send, recv),
        )
        .await
        {
            tracing::debug!(%error, "diagnostic lane ended");
        }
    }
}
impl DiagnosticReports {
    pub(crate) async fn collect(&self, session: SessionId) -> Result<DiagnosticCollection> {
        let Some((owner, connection)) = self.route(session) else {
            return Ok(DiagnosticCollection {
                bundle: self
                    .get(session)
                    .context("diagnostic session unavailable or evicted")?,
                status: CollectionStatus::Offline,
            });
        };
        diagnostics::collect(&self.archive, &owner, session, async {
            let (send, recv) = connection.open_bi().await?;
            Ok(PacketLane::new(send, recv))
        })
        .await
    }
}
#[cfg(test)]
mod tests;
