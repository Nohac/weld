//! Iroh correlation IDs and connection routes for the shared diagnostic archive.
use crate::{IrohPeerIdentity, pairing::DiagnosticPermission};
use anyhow::Result;
use iroh::{
    EndpointId,
    endpoint::{Connection, WeakConnectionHandle},
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use weld_diagnostics::{
    Endpoint, Recorder, Report, ReportBundle, SessionId, archive::ReportArchive,
};

mod remote;
pub(crate) use remote::serve;
pub use weld_diagnostics::collection::{CollectionStatus, DiagnosticCollection};

const MAX_ROUTES: usize = 16;
#[derive(Clone, Default)]
pub struct DiagnosticReports {
    archive: ReportArchive<EndpointId>,
    routes: Arc<Mutex<VecDeque<(EndpointId, WeakConnectionHandle)>>>,
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
        // Both peers derive a public correlation ID from this authenticated connection.
        connection
            .export_keying_material(&mut id, b"EXPORTER-weld-public-diagnostic-id", b"v1")
            .ok()?;
        let owner = connection.remote_id();
        let recorder = self
            .archive
            .start(owner, SessionId(id), endpoint, move || {
                access.permits(owner)
            })?;
        let mut routes = self.routes.lock().ok()?;
        routes.retain(|(_, route)| {
            route
                .upgrade()
                .is_some_and(|connection| connection.close_reason().is_none())
        });
        if routes.len() == MAX_ROUTES {
            routes.pop_front();
        }
        routes.push_back((owner, connection.weak_handle()));
        Some(recorder)
    }
    pub fn restore_receiver_report(&self, owner: &IrohPeerIdentity, report: Report) -> Result<()> {
        self.archive
            .restore_receiver_report(owner.as_str().parse()?, report)
            .map_err(anyhow::Error::msg)
    }
    pub fn list(&self) -> Vec<ReportBundle> {
        self.archive.list()
    }
    pub fn get(&self, session: SessionId) -> Option<ReportBundle> {
        self.archive.get(session)
    }
    fn route(&self, session: SessionId) -> Option<(EndpointId, Connection)> {
        let owner = self.archive.owner(session)?;
        self.routes
            .lock()
            .ok()?
            .iter()
            .rev()
            .filter(|(peer, _)| *peer == owner)
            .find_map(|(_, route)| {
                let connection = route.upgrade()?;
                connection
                    .close_reason()
                    .is_none()
                    .then_some((owner, connection))
            })
    }
    pub(crate) fn exchange(&self, owner: EndpointId, report: Report) -> Result<Report> {
        self.archive
            .exchange(&owner, report)
            .map_err(|status| anyhow::anyhow!("{status}"))
    }
}
