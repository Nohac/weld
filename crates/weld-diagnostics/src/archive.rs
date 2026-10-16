//! Bounded session evidence with authenticated ownership and live access checks.

use crate::{Endpoint, Recorder, Report, ReportBundle, SessionId, collection::CollectionStatus};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

const MAX_REPORTS: usize = 16;
struct Entry<P> {
    owner: P,
    recorder: Recorder,
    peer: Option<Report>,
    permitted: Arc<dyn Fn() -> bool + Send + Sync>,
}

/// Shared report storage. A listener supplies its authenticated owner key and
/// current consent lookup; archive methods enforce session and endpoint scope.
pub struct ReportArchive<P>(Arc<Mutex<VecDeque<Entry<P>>>>);
impl<P> Clone for ReportArchive<P> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<P> Default for ReportArchive<P> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(VecDeque::new())))
    }
}
impl<P: Eq + Clone> ReportArchive<P> {
    pub fn start(
        &self,
        owner: P,
        session: SessionId,
        endpoint: Endpoint,
        permitted: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Option<Recorder> {
        let recorder = Recorder::new(session, endpoint);
        let mut entries = self.0.lock().ok()?;
        if entries
            .iter()
            .any(|entry| entry.recorder.session() == Some(session))
        {
            return None;
        }
        if entries.len() == MAX_REPORTS {
            entries.pop_front();
        }
        entries.push_back(Entry {
            owner,
            recorder: recorder.clone(),
            peer: None,
            permitted: Arc::new(permitted),
        });
        Some(recorder)
    }
    pub fn restore_receiver_report(&self, owner: P, report: Report) -> Result<(), &'static str> {
        if report.endpoint != Endpoint::Receiver {
            return Err("expected a local receiver report");
        }
        let session = report.session;
        let recorder = Recorder::from_finished_report(report)?;
        let mut entries = self
            .0
            .lock()
            .map_err(|_| "diagnostic archive unavailable")?;
        if let Some(existing) = entries
            .iter()
            .find(|entry| entry.recorder.session() == Some(session))
        {
            return if existing.owner == owner {
                Ok(())
            } else {
                Err("diagnostic session owner differs")
            };
        }
        if entries.len() == MAX_REPORTS {
            entries.pop_front();
        }
        entries.push_back(Entry {
            owner,
            recorder,
            peer: None,
            permitted: Arc::new(|| true),
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
    pub fn owner(&self, session: SessionId) -> Option<P> {
        self.0
            .lock()
            .ok()?
            .iter()
            .find(|entry| entry.recorder.session() == Some(session))
            .map(|entry| entry.owner.clone())
    }
    pub fn local_for_peer(
        &self,
        owner: &P,
        session: SessionId,
    ) -> Result<Report, CollectionStatus> {
        let (recorder, permitted) = {
            let entries = self.0.lock().map_err(|_| CollectionStatus::Unavailable)?;
            let entry = entries
                .iter()
                .find(|entry| &entry.owner == owner && entry.recorder.session() == Some(session))
                .ok_or(CollectionStatus::Unavailable)?;
            (entry.recorder.clone(), entry.permitted.clone())
        };
        if !permitted() {
            return Err(CollectionStatus::Denied);
        }
        let report = recorder.snapshot().ok_or(CollectionStatus::Unavailable)?;
        if !permitted() {
            return Err(CollectionStatus::Denied);
        }
        Ok(report)
    }
    /// Validate and retain peer evidence collected for a locally requested report.
    pub fn cache_peer(&self, owner: &P, report: Report) -> Result<(), &'static str> {
        report.validate()?;
        let mut entries = self
            .0
            .lock()
            .map_err(|_| "diagnostic archive unavailable")?;
        let entry = entries
            .iter_mut()
            .find(|entry| &entry.owner == owner && entry.recorder.session() == Some(report.session))
            .ok_or("no diagnostic session shared with this peer")?;
        let local = entry
            .recorder
            .snapshot()
            .ok_or("diagnostic report unavailable")?;
        if report.endpoint == local.endpoint {
            return Err("peer claimed the local report role");
        }
        entry.peer = Some(report);
        Ok(())
    }
    /// Exchange evidence after checking the requester's current report permission.
    pub fn exchange(&self, owner: &P, report: Report) -> Result<Report, CollectionStatus> {
        let session = report.session;
        self.local_for_peer(owner, session)?;
        self.cache_peer(owner, report)
            .map_err(|_| CollectionStatus::Failed)?;
        self.local_for_peer(owner, session)
    }
}

#[cfg(test)]
mod tests;
