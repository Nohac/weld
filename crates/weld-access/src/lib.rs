//! Live access-session authority shared by local administration and application relays.
//!
//! Revocation permanently invalidates a session's permit. Explicit local admission
//! reopening permits fresh sessions while keeping every retired permit invalid.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

const MAX_SESSIONS: usize = 256;

/// Process-local administrative selector. Authorization is carried by [`AccessSession`].
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccessSessionId(pub u64);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: AccessSessionId,
    /// Stable identity supplied by the trusted transport or local assembly.
    pub participant: String,
    pub authorized: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccessStatus {
    pub accepting: bool,
    pub blocked_participants: Vec<String>,
    pub sessions: Vec<SessionInfo>,
}

struct Session {
    id: AccessSessionId,
    participant: String,
    authorized: AtomicBool,
}

/// A revocable permit held by one connection and its source relay.
#[derive(Clone)]
pub struct AccessSession(Arc<Session>);

impl AccessSession {
    pub fn id(&self) -> AccessSessionId {
        self.0.id
    }

    pub fn participant(&self) -> &str {
        &self.0.participant
    }

    /// Cheap check at input acceptance and frame-publication boundaries.
    pub fn is_authorized(&self) -> bool {
        self.0.authorized.load(Ordering::Acquire)
    }

    /// Retires this connection without preventing a future authorized connection.
    pub fn end(&self) {
        self.0.authorized.store(false, Ordering::Release);
    }
}

struct Registry {
    accepting: bool,
    blocked_participants: BTreeSet<String>,
    sessions: BTreeMap<AccessSessionId, Weak<Session>>,
    next: Option<u64>,
}

impl Drop for Registry {
    fn drop(&mut self) {
        for session in self.sessions.values().filter_map(Weak::upgrade) {
            session.authorized.store(false, Ordering::Release);
        }
    }
}

/// The local administrator owns admission and revocation. Transports register
/// sessions after authenticating their participant and checking its admission policy.
#[derive(Clone)]
pub struct AccessAuthority {
    registry: Arc<Mutex<Registry>>,
    notify: Arc<dyn Fn() + Send + Sync>,
}

impl AccessAuthority {
    pub fn new(notify: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            registry: Arc::new(Mutex::new(Registry {
                accepting: true,
                blocked_participants: BTreeSet::new(),
                sessions: BTreeMap::new(),
                next: Some(1),
            })),
            notify: Arc::new(notify),
        }
    }

    pub fn open(&self, participant: String) -> Result<AccessSession> {
        validate_participant(&participant)?;
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("access authority unavailable"))?;
        let authorized =
            registry.accepting && !registry.blocked_participants.contains(&participant);
        ensure!(
            authorized,
            "access revoked; explicit local authorization is required"
        );
        registry
            .sessions
            .retain(|_, session| session.strong_count() > 0);
        ensure!(
            registry.sessions.len() < MAX_SESSIONS,
            "access session capacity reached"
        );
        let raw = registry
            .next
            .context("access session identities exhausted")?;
        registry.next = raw.checked_add(1);
        let session = Arc::new(Session {
            id: AccessSessionId(raw),
            participant,
            authorized: AtomicBool::new(authorized),
        });
        registry
            .sessions
            .insert(session.id, Arc::downgrade(&session));
        Ok(AccessSession(session))
    }

    pub fn status(&self) -> Result<AccessStatus> {
        let registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("access authority unavailable"))?;
        Ok(AccessStatus {
            accepting: registry.accepting,
            blocked_participants: registry.blocked_participants.iter().cloned().collect(),
            sessions: registry
                .sessions
                .values()
                .filter_map(Weak::upgrade)
                .map(|session| SessionInfo {
                    id: session.id,
                    participant: session.participant.clone(),
                    authorized: session.authorized.load(Ordering::Acquire),
                })
                .collect(),
        })
    }

    /// Stops one session and prevents its participant reconnecting automatically.
    pub fn revoke(&self, id: AccessSessionId) -> Result<()> {
        {
            let mut registry = self
                .registry
                .lock()
                .map_err(|_| anyhow::anyhow!("access authority unavailable"))?;
            let session = registry
                .sessions
                .get(&id)
                .and_then(Weak::upgrade)
                .context("access session unavailable")?;
            registry
                .blocked_participants
                .insert(session.participant.clone());
            session.authorized.store(false, Ordering::Release);
        }
        (self.notify)();
        Ok(())
    }

    /// Stops all sessions and latches admission closed before waking consumers.
    pub fn revoke_all(&self) -> Result<()> {
        {
            let mut registry = self
                .registry
                .lock()
                .map_err(|_| anyhow::anyhow!("access authority unavailable"))?;
            registry.accepting = false;
            registry.blocked_participants.clear();
            for session in registry.sessions.values().filter_map(Weak::upgrade) {
                session.authorized.store(false, Ordering::Release);
            }
        }
        (self.notify)();
        Ok(())
    }

    /// Explicit local consent to admit fresh sessions under the current admission policy.
    pub fn allow_new_sessions(&self) -> Result<()> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("access authority unavailable"))?;
        registry.accepting = true;
        registry.blocked_participants.clear();
        Ok(())
    }
}

fn validate_participant(participant: &str) -> Result<()> {
    ensure!(
        !participant.is_empty()
            && participant.len() <= 160
            && !participant.chars().any(char::is_control),
        "invalid access participant"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn global_revocation_blocks_reconnect_and_reopening_never_revives_old_permits() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let count = wakes.clone();
        let authority = AccessAuthority::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        });
        let first = authority.open("first".into()).expect("first");
        let second = authority.open("second".into()).expect("second");
        authority.revoke_all().expect("revoke");
        assert!(!first.is_authorized() && !second.is_authorized());
        assert!(authority.open("first".into()).is_err());
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        authority.allow_new_sessions().expect("allow");
        let fresh = authority.open("first".into()).expect("fresh");
        assert!(fresh.is_authorized());
        assert_ne!(fresh.id(), first.id());
        assert!(!first.is_authorized() && !second.is_authorized());
    }

    #[test]
    fn scoped_revocation_preserves_other_participants_and_ended_sessions_retire() {
        let authority = AccessAuthority::new(|| {});
        let first = authority.open("first".into()).expect("first");
        let second = authority.open("second".into()).expect("second");
        authority.revoke(first.id()).expect("revoke");
        assert!(!first.is_authorized());
        assert!(second.is_authorized());
        assert!(authority.open("first".into()).is_err());
        second.end();
        assert!(!second.is_authorized());
        assert!(authority.open("second".into()).is_ok());
        drop(first);
        drop(second);
        assert!(authority.status().expect("status").sessions.is_empty());
    }

    #[test]
    fn authority_teardown_invalidates_every_retained_permit() {
        let authority = AccessAuthority::new(|| {});
        let session = authority.open("test".into()).expect("session");
        let second_owner = authority.clone();
        drop(authority);
        assert!(session.is_authorized());
        drop(second_owner);
        assert!(!session.is_authorized());
        let restarted = AccessAuthority::new(|| {});
        assert!(
            restarted
                .status()
                .expect("fresh status")
                .sessions
                .is_empty()
        );
        let fresh = restarted.open("test".into()).expect("fresh admission");
        assert!(fresh.is_authorized());
        assert!(!session.is_authorized());
    }
}
