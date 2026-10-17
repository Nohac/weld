//! Independent diagnostic stream admission and authenticated peer routing.
use super::{CollectionStatus, DiagnosticCollection, DiagnosticReports};
use crate::channel::PacketLane;
use anyhow::{Context, Result};
use iroh::endpoint::Connection;
use weld_diagnostics::SessionId;
use weld_hoist_core::session::diagnostics;

impl DiagnosticReports {
    pub(crate) fn listen(
        &self,
        tasks: &weld_session::SessionTasks,
        connection: Connection,
    ) -> Result<()> {
        diagnostics::listen(
            tasks,
            self.archive.clone(),
            connection.remote_id(),
            move || {
                let connection = connection.clone();
                async move {
                    let (send, recv) = connection.accept_bi().await?;
                    Ok(PacketLane::new(send, recv))
                }
            },
        )
    }
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
