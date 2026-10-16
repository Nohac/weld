use super::*;
use std::sync::Mutex;
use weld_access::AccessAuthority;

#[derive(Clone, Default)]
struct MemoryStore {
    devices: Arc<Mutex<BTreeMap<String, PairedDevice>>>,
    fail: Arc<AtomicBool>,
}
impl TrustStorage for MemoryStore {
    fn load(&self) -> Result<BTreeMap<String, PairedDevice>> {
        Ok(self.devices.lock().expect("store").clone())
    }
    fn save(&self, devices: &BTreeMap<String, PairedDevice>) -> Result<()> {
        ensure!(!self.fail.load(Ordering::Acquire), "storage failure");
        *self.devices.lock().expect("store") = devices.clone();
        Ok(())
    }
}
struct Connection(Arc<AtomicBool>);
impl ConnectionControl for Connection {
    fn is_connected(&self) -> bool {
        !self.0.load(Ordering::Acquire)
    }
    fn close(&self) {
        self.0.store(true, Ordering::Release);
    }
}
fn grant() -> DevicePermissions {
    DevicePermissions {
        browse: true,
        hoist: true,
        thumbnails: true,
        diagnostics: false,
    }
}
fn peer() -> AuthenticatedPeer {
    AuthenticatedPeer::new("test", "peer").expect("identity")
}
fn authority(store: MemoryStore) -> PairingAuthority {
    PairingAuthority::start("test".into(), move || Ok(store)).expect("authority")
}
fn enroll(authority: &PairingAuthority) -> PairingClaim {
    let token = authority.invite().expect("invitation");
    authority
        .claim(peer(), token, "Phone".into())
        .expect("claim")
        .wait_blocking()
        .expect("response")
        .expect("claimed")
}

#[test]
fn confirmation_persists_before_authorizing_and_rejects_other_identity_namespaces() {
    let store = MemoryStore::default();
    let authority = authority(store.clone());
    let claim = enroll(&authority);
    let candidate = &claim.candidate;
    assert!(
        authority
            .approve(candidate.identity.clone(), "wrong".into(), grant())
            .is_err()
    );
    store.fail.store(true, Ordering::Release);
    assert!(
        authority
            .approve(
                candidate.identity.clone(),
                candidate.verification.clone(),
                grant()
            )
            .is_err()
    );
    assert!(authority.permissions(&peer()).is_none());
    store.fail.store(false, Ordering::Release);
    authority
        .approve(
            candidate.identity.clone(),
            candidate.verification.clone(),
            grant(),
        )
        .expect("approve");
    assert!(claim.approval.wait_blocking().expect("confirmed"));
    assert_eq!(authority.permissions(&peer()), Some(grant()));
    assert!(
        authority
            .permissions(&AuthenticatedPeer::new("other", "peer").expect("identity"))
            .is_none()
    );
    let access_owner = AccessAuthority::new(|| {});
    let access = access_owner.open("peer".into()).expect("access");
    let closed = Arc::new(AtomicBool::new(false));
    assert!(
        authority
            .admit(
                AuthenticatedPeer::new("other", "peer").expect("identity"),
                access.clone(),
                Connection(closed.clone())
            )
            .expect("admit")
            .wait_blocking()
            .expect("response")
            .is_err()
    );
    assert!(closed.load(Ordering::Acquire));
    drop(authority);
    let restored = PairingAuthority::start("test".into(), move || Ok(store)).expect("restart");
    assert_eq!(restored.permissions(&peer()), Some(grant()));
}

#[test]
fn revocation_retires_admitted_input_even_when_persistence_fails() {
    let store = MemoryStore::default();
    let authority = authority(store.clone());
    let claim = enroll(&authority);
    authority
        .approve(
            claim.candidate.identity,
            claim.candidate.verification,
            grant(),
        )
        .expect("approve");
    let access_owner = AccessAuthority::new(|| {});
    let access = access_owner.open("peer".into()).expect("permit");
    let closed = Arc::new(AtomicBool::new(false));
    authority
        .admit(peer(), access.clone(), Connection(closed.clone()))
        .expect("admit")
        .wait_blocking()
        .expect("response")
        .expect("admitted");
    store.fail.store(true, Ordering::Release);
    assert!(
        authority
            .revoke("peer".into())
            .expect_err("failed save")
            .to_string()
            .contains("revoked for this session")
    );
    assert!(!access.is_authorized());
    assert!(closed.load(Ordering::Acquire));
    assert!(authority.permissions(&peer()).is_none());
    assert!(store.load().expect("durable state").contains_key("peer"));
    store.fail.store(false, Ordering::Release);
    authority.revoke("peer".into()).expect("retry persistence");
    assert!(
        !store
            .load()
            .expect("persisted revocation")
            .contains_key("peer")
    );
}

#[test]
fn expired_cancelled_disconnected_and_replayed_claims_cannot_be_approved() {
    let now = Instant::now();
    let snapshot = Arc::new(Published {
        namespace: "test".into(),
        stopped: AtomicBool::new(false),
        devices: RwLock::new(Arc::new(BTreeMap::new())),
    });
    let mut state = State {
        storage: Box::new(MemoryStore::default()),
        devices: BTreeMap::new(),
        unsaved_revocations: BTreeSet::new(),
        invitation: None,
        admissions: Vec::new(),
        published: snapshot,
    };
    let token = state.invite(now).expect("invitation");
    assert!(state.claim(peer(), [0; 32], "Phone".into()).is_err());
    let claim = state.claim(peer(), token, "Phone".into()).expect("claim");
    assert!(state.claim(peer(), token, "Replay".into()).is_err());
    let candidate = claim.candidate;
    drop(claim.approval);
    assert!(
        state
            .approve(&candidate.identity, &candidate.verification, grant())
            .is_err()
    );
    let token = state.invite(now).expect("invitation");
    state.expire(now + INVITATION_LIFETIME);
    assert!(state.claim(peer(), token, "Phone".into()).is_err());
    let service = authority(MemoryStore::default());
    let claim = enroll(&service);
    service.cancel().expect("cancel");
    assert!(claim.approval.wait_blocking().is_err());
    assert!(
        service
            .approve(
                claim.candidate.identity,
                claim.candidate.verification,
                grant()
            )
            .is_err()
    );
}

#[test]
fn cancelled_admission_and_shutdown_retire_connection_permits() {
    let authority = authority(MemoryStore::default());
    let claim = enroll(&authority);
    authority
        .approve(
            claim.candidate.identity,
            claim.candidate.verification,
            grant(),
        )
        .expect("approve");
    let owner = AccessAuthority::new(|| {});
    let cancelled = owner.open("cancelled".into()).expect("permit");
    cancelled.end();
    let closed = Arc::new(AtomicBool::new(false));
    assert!(
        authority
            .admit(peer(), cancelled, Connection(closed.clone()))
            .expect("admit")
            .wait_blocking()
            .expect("response")
            .is_err()
    );
    assert!(closed.load(Ordering::Acquire));
    let active = owner.open("active".into()).expect("permit");
    let closed = Arc::new(AtomicBool::new(false));
    authority
        .admit(peer(), active.clone(), Connection(closed.clone()))
        .expect("admit")
        .wait_blocking()
        .expect("response")
        .expect("admitted");
    drop(authority);
    assert!(!active.is_authorized());
    assert!(closed.load(Ordering::Acquire));
}
