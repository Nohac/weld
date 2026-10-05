//! Local bounded archive with authenticated ownership checks for peer exchange.
use anyhow::{Context, Result, ensure};
use iroh::{EndpointId, endpoint::Connection};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use weld_diagnostics::{Endpoint, Recorder, Report, ReportBundle, SessionId};

const MAX_REPORTS: usize = 16;
#[derive(Clone, Default)]
pub struct DiagnosticReports(Arc<Mutex<VecDeque<Entry>>>);
struct Entry {
    owner: EndpointId,
    recorder: Recorder,
    peer: Option<Report>,
}

impl DiagnosticReports {
    pub(crate) fn start(&self, connection: &Connection, endpoint: Endpoint) -> Option<Recorder> {
        let mut id = [0; 16];
        // A dedicated TLS-exporter label supplies a public correlation ID shared
        // by both peers. These bytes are never used as an authorization secret.
        connection
            .export_keying_material(&mut id, b"EXPORTER-weld-public-diagnostic-id", b"v1")
            .ok()?;
        let recorder = Recorder::new(SessionId(id), endpoint);
        let mut entries = self.0.lock().ok()?;
        if entries.len() == MAX_REPORTS {
            entries.pop_front();
        }
        entries.push_back(Entry {
            owner: connection.remote_id(),
            recorder: recorder.clone(),
            peer: None,
        });
        Some(recorder)
    }
    pub fn list(&self) -> Vec<ReportBundle> {
        self.0
            .lock()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        Some(ReportBundle {
                            local: entry.recorder.snapshot()?,
                            peer: entry.peer.clone(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn get(&self, session: SessionId) -> Option<ReportBundle> {
        let entries = self.0.lock().ok()?;
        let entry = entries
            .iter()
            .find(|entry| entry.recorder.session() == Some(session))?;
        Some(ReportBundle {
            local: entry.recorder.snapshot()?,
            peer: entry.peer.clone(),
        })
    }
    /// Called only after the session's explicit diagnostics permission check.
    pub(crate) fn exchange(&self, owner: EndpointId, report: Report) -> Result<Report> {
        report.validate().map_err(anyhow::Error::msg)?;
        let mut entries = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("diagnostic archive unavailable"))?;
        let entry = entries
            .iter_mut()
            .find(|entry| entry.owner == owner && entry.recorder.session() == Some(report.session))
            .context("no diagnostic session shared with this peer")?;
        let local = entry
            .recorder
            .snapshot()
            .context("diagnostic report unavailable")?;
        ensure!(
            report.endpoint != local.endpoint,
            "peer claimed the local report role"
        );
        entry.peer = Some(report);
        Ok(local)
    }
}
