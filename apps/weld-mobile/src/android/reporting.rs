//! Phone-local evidence survives disconnects and app restarts in private storage.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, io::Read, path::PathBuf, sync::Mutex};
use weld_diagnostics::{Cause, Observation, Recorder, Report, ReportBundle, SessionId};

#[derive(Clone, Serialize, Deserialize)]
struct Saved {
    owner: String,
    bundle: ReportBundle,
}
#[derive(Default)]
struct State {
    path: Option<PathBuf>,
    current: Option<(String, Recorder)>,
    saved: Option<Saved>,
    persisted: Option<SessionId>,
    notice: String,
}
#[derive(Default)]
pub(super) struct Reports(Mutex<State>);
impl Reports {
    pub fn open(&self, directory: PathBuf) {
        let path = directory.join("diagnostics.json");
        let saved = (|| -> Result<Saved> {
            let mut bytes = Vec::new();
            fs::File::open(&path)?
                .take(weld_diagnostics::MAX_EXPORT_BYTES + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= weld_diagnostics::MAX_EXPORT_BYTES,
                "saved report too large"
            );
            let saved: Saved = serde_json::from_slice(&bytes)?;
            saved.bundle.validate().map_err(anyhow::Error::msg)?;
            Ok(saved)
        })()
        .ok();
        if let Ok(mut state) = self.0.lock() {
            state.path = Some(path);
            state.saved = saved;
        }
    }
    pub fn attach(&self, owner: String, recorder: Recorder) {
        if let Ok(mut state) = self.0.lock() {
            state.current = Some((owner, recorder));
        }
    }
    pub fn recorder(&self) -> Option<Recorder> {
        self.0
            .lock()
            .ok()?
            .current
            .as_ref()
            .map(|(_, recorder)| recorder.clone())
    }
    pub fn persist_finished(&self) -> Result<()> {
        let save = {
            let mut state = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("report state unavailable"))?;
            let Some((owner, recorder)) = &state.current else {
                return Ok(());
            };
            let Some(session) = recorder.finished_session() else {
                return Ok(());
            };
            if state.persisted == Some(session) {
                return Ok(());
            }
            let Some(report) = recorder.snapshot() else {
                return Ok(());
            };
            if state.saved.as_ref().is_some_and(|saved| {
                saved.owner == *owner
                    && (saved.bundle.peer.is_some() || has_failure(&saved.bundle.local))
                    && !has_failure(&report)
            }) {
                // A clean collection connection must not replace the incident
                // that the user just retrieved from the other endpoint.
                state.persisted = Some(report.session);
                return Ok(());
            }
            let saved = Saved {
                owner: owner.clone(),
                bundle: ReportBundle {
                    local: report,
                    peer: None,
                },
            };
            state.saved = Some(saved.clone());
            (state.path.clone(), saved)
        };
        if let (Some(path), saved) = &save {
            save_report(path.clone(), saved)?;
        }
        if let Ok(mut state) = self.0.lock() {
            state.persisted = Some(save.1.bundle.local.session);
        }
        Ok(())
    }
    fn selected(state: &State) -> Option<Saved> {
        if let Some(saved) = &state.saved {
            return Some(saved.clone());
        }
        let (owner, recorder) = state.current.as_ref()?;
        Some(Saved {
            owner: owner.clone(),
            bundle: ReportBundle {
                local: recorder.snapshot()?,
                peer: None,
            },
        })
    }
    pub fn for_peer(&self, owner: &str) -> Result<Report> {
        let state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("report state unavailable"))?;
        let saved = Self::selected(&state).context("no session report yet")?;
        ensure!(
            saved.owner == owner,
            "report belongs to a different host; reconnect to that host to collect it"
        );
        Ok(saved.bundle.local)
    }
    pub fn merge(&self, peer: Report) -> Result<()> {
        let save = {
            let mut state = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("report state unavailable"))?;
            let mut saved = Self::selected(&state).context("no local session report")?;
            saved.bundle.peer = Some(peer);
            saved.bundle.validate().map_err(anyhow::Error::msg)?;
            state.saved = Some(saved.clone());
            state.notice = "Peer evidence collected. Timelines use separate clocks.".into();
            state.path.clone().map(|path| (path, saved))
        };
        if let Some((path, saved)) = save {
            save_report(path, &saved)?;
        }
        Ok(())
    }
    pub fn notice(&self, message: impl Into<String>) {
        if let Ok(mut state) = self.0.lock() {
            state.notice = message.into();
        }
    }
    pub fn text(&self) -> String {
        let Ok(state) = self.0.lock() else {
            return "Report unavailable".into();
        };
        let Some(saved) = Self::selected(&state) else {
            return "No session evidence has been recorded yet.".into();
        };
        format!(
            "Session {}\n{}\n{}",
            saved.bundle.local.session,
            state.notice,
            weld_diagnostics::explain(&saved.bundle).render(false)
        )
    }
}

fn has_failure(report: &Report) -> bool {
    report.first_failure.as_ref().is_some_and(|event| {
        matches!(event.observation, Observation::Failure { cause, .. }
            if !matches!(cause, Cause::PeerClosed | Cause::LocalShutdown))
    })
}

fn save_report(path: PathBuf, saved: &Saved) -> Result<()> {
    saved.bundle.validate().map_err(anyhow::Error::msg)?;
    let bytes = serde_json::to_vec(saved)?;
    ensure!(
        bytes.len() as u64 <= weld_diagnostics::MAX_EXPORT_BYTES,
        "saved report exceeds bound"
    );
    let temporary = path.with_extension("pending");
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_diagnostics::{Cause, Endpoint, Observation, Operation};
    #[test]
    fn offline_report_survives_restart_and_collection_never_crosses_hosts() {
        let directory = tempfile::tempdir().expect("directory");
        let reports = Reports::default();
        reports.open(directory.path().to_owned());
        let id = SessionId([8; 16]);
        let recorder = Recorder::new(id, Endpoint::Receiver);
        recorder.record(Observation::Failure {
            operation: Operation::SessionRead,
            cause: Cause::Timeout,
        });
        recorder.record(Observation::Ended);
        reports.attach("approved-host".into(), recorder);
        assert!(reports.recorder().is_some());
        reports.persist_finished().expect("save");
        drop(reports);
        let reopened = Reports::default();
        reopened.open(directory.path().to_owned());
        assert!(reopened.text().contains("deadline expired"));
        assert!(reopened.for_peer("another-host").is_err());
        assert_eq!(
            reopened
                .for_peer("approved-host")
                .expect("own report")
                .session,
            id
        );
        let mut peer = Recorder::new(SessionId([9; 16]), Endpoint::Source)
            .snapshot()
            .expect("peer");
        assert!(reopened.merge(peer.clone()).is_err());
        peer.session = id;
        reopened.merge(peer).expect("same session");
        reopened.notice("Collected");
        assert!(reopened.text().contains("Endpoint clocks are independent"));
        assert!(!reopened.text().contains("Peer evidence is missing"));
        let collection = Recorder::new(SessionId([10; 16]), Endpoint::Receiver);
        collection.record(Observation::Ended);
        reopened.attach("approved-host".into(), collection);
        reopened
            .persist_finished()
            .expect("collection session ended");
        let restarted = Reports::default();
        restarted.open(directory.path().to_owned());
        assert_eq!(
            restarted
                .for_peer("approved-host")
                .expect("incident")
                .session,
            id
        );
        assert!(!restarted.text().contains("Peer evidence is missing"));
    }

    #[test]
    fn failed_disk_save_is_retried() {
        let directory = tempfile::tempdir().expect("directory");
        let storage = directory.path().join("not-yet-mounted");
        let reports = Reports::default();
        reports.open(storage.clone());
        let recorder = Recorder::new(SessionId([1; 16]), Endpoint::Receiver);
        recorder.record(Observation::Ended);
        reports.attach("host".into(), recorder);
        assert!(reports.persist_finished().is_err());
        fs::create_dir(&storage).expect("storage becomes available");
        reports.persist_finished().expect("retry");
        assert!(storage.join("diagnostics.json").exists());
    }
}
