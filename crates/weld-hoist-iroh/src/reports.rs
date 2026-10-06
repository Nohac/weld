//! Local bounded archive with authenticated ownership checks for peer exchange.
use crate::{IrohPeerIdentity, pairing::DiagnosticPermission};
use anyhow::{Context, Result, ensure};
use iroh::{
    EndpointId,
    endpoint::{Connection, WeakConnectionHandle},
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use weld_diagnostics::{Endpoint, Recorder, Report, ReportBundle, SessionId};

mod remote;
pub(crate) use remote::serve;
pub use remote::{CollectionStatus, DiagnosticCollection};

const MAX_REPORTS: usize = 16;
#[derive(Clone, Default)]
pub struct DiagnosticReports(Arc<Mutex<VecDeque<Entry>>>);
struct Entry {
    owner: EndpointId,
    recorder: Recorder,
    peer: Option<Report>,
    connection: Option<WeakConnectionHandle>,
    access: Access,
}

#[derive(Clone)]
pub(crate) enum Access {
    /// A locally approved source or explicitly trusted development peer.
    Participant,
    /// Desktop grants are checked again for each request, including old sessions.
    Paired(DiagnosticPermission),
}
impl Access {
    fn permits(&self, owner: EndpointId) -> bool {
        match self {
            Self::Participant => true,
            Self::Paired(permission) => permission.permits(owner),
        }
    }
}

impl DiagnosticReports {
    pub(crate) fn start(
        &self,
        connection: &Connection,
        endpoint: Endpoint,
        access: Access,
    ) -> Option<Recorder> {
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
            connection: Some(connection.weak_handle()),
            access,
        });
        Some(recorder)
    }
    /// Restore this receiver's private persisted evidence before reconnecting.
    pub fn restore_receiver_report(&self, owner: &IrohPeerIdentity, report: Report) -> Result<()> {
        ensure!(
            report.endpoint == Endpoint::Receiver,
            "expected a local receiver report"
        );
        let owner: EndpointId = owner.as_str().parse()?;
        let session = report.session;
        let recorder = Recorder::from_finished_report(report).map_err(anyhow::Error::msg)?;
        let mut entries = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("diagnostic archive unavailable"))?;
        if let Some(existing) = entries
            .iter()
            .find(|entry| entry.recorder.session() == Some(session))
        {
            ensure!(existing.owner == owner, "diagnostic session owner differs");
            return Ok(());
        }
        if entries.len() == MAX_REPORTS {
            entries.pop_front();
        }
        entries.push_back(Entry {
            owner,
            recorder,
            peer: None,
            connection: None,
            access: Access::Participant,
        });
        Ok(())
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
    fn local_for_peer(
        &self,
        owner: EndpointId,
        session: SessionId,
    ) -> Result<Report, CollectionStatus> {
        let entries = self.0.lock().map_err(|_| CollectionStatus::Unavailable)?;
        let entry = entries
            .iter()
            .find(|entry| entry.owner == owner && entry.recorder.session() == Some(session))
            .ok_or(CollectionStatus::Unavailable)?;
        if !entry.access.permits(owner) {
            return Err(CollectionStatus::Denied);
        }
        entry
            .recorder
            .snapshot()
            .ok_or(CollectionStatus::Unavailable)
    }
    fn route(&self, session: SessionId) -> Option<(EndpointId, Connection)> {
        let entries = self.0.lock().ok()?;
        let owner = entries
            .iter()
            .find(|entry| entry.recorder.session() == Some(session))?
            .owner;
        // An old session can be collected after reconnecting to the same identity.
        entries
            .iter()
            .rev()
            .filter(|entry| entry.owner == owner)
            .find_map(|entry| {
                let connection = entry.connection.as_ref()?.upgrade()?;
                connection
                    .close_reason()
                    .is_none()
                    .then_some((owner, connection))
            })
    }
    /// Cache authenticated peer evidence after validating session ownership and role.
    /// Serving a local report additionally requires the caller's access-policy check.
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
