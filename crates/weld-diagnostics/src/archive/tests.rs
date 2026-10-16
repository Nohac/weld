use super::*;
use crate::collection::RequestBudget;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[test]
fn archive_scopes_owner_role_live_consent_and_eviction() {
    let archive = ReportArchive::default();
    let allowed = Arc::new(AtomicBool::new(true));
    let gate = allowed.clone();
    let session = SessionId([1; 16]);
    let recorder = archive
        .start("owner", session, Endpoint::Source, move || {
            gate.load(Ordering::Acquire)
        })
        .expect("start");
    assert!(archive.local_for_peer(&"stranger", session).is_err());
    let mut report = recorder.snapshot().expect("report");
    assert!(archive.exchange(&"owner", report.clone()).is_err());
    report.endpoint = Endpoint::Receiver;
    assert!(archive.exchange(&"stranger", report.clone()).is_err());
    archive
        .exchange(&"owner", report.clone())
        .expect("peer evidence");
    allowed.store(false, Ordering::Release);
    assert!(matches!(
        archive.exchange(&"owner", report.clone()),
        Err(CollectionStatus::Denied)
    ));
    assert!(matches!(
        archive.local_for_peer(&"owner", session),
        Err(CollectionStatus::Denied)
    ));
    assert!(archive.get(session).expect("cached").peer.is_some());
    assert!(archive.restore_receiver_report("stranger", report).is_err());
    for value in 2..=17 {
        archive
            .start("owner", SessionId([value; 16]), Endpoint::Source, || true)
            .expect("start");
    }
    assert_eq!(archive.list().len(), MAX_REPORTS);
    assert!(archive.get(session).is_none());
    let mut budget = RequestBudget::default();
    let now = Instant::now();
    assert!(budget.admit(now));
    assert!(!budget.admit(now + Duration::from_millis(999)));
    assert!(budget.admit(now + Duration::from_secs(1)));
}
