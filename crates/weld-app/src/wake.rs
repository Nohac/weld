//! Coalesced requests for an application-policy tick from external workers.
use bevy::prelude::Resource;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use weld_core::host::{ClientRuntimeNotifier, ClientRuntimeWakeSource, client_runtime_notifier};

#[derive(Clone, Resource)]
pub struct PolicyWake {
    pending: Arc<AtomicBool>,
    notifier: ClientRuntimeNotifier,
}
impl PolicyWake {
    pub(crate) fn new() -> io::Result<(Self, ClientRuntimeWakeSource)> {
        let (notifier, source) = client_runtime_notifier()?;
        Ok((
            Self {
                pending: Arc::new(AtomicBool::new(false)),
                notifier,
            },
            source,
        ))
    }
    pub fn notify(&self) -> io::Result<()> {
        self.pending.store(true, Ordering::Release);
        self.notifier.notify()
    }
    pub(crate) fn take(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn external_work_coalesces_without_creating_idle_policy_demand() {
        let (wake, _source) = PolicyWake::new().expect("eventfd");
        assert!(!wake.take());
        for _ in 0..20 {
            wake.clone().notify().expect("wake");
        }
        assert!(wake.take());
        assert!(!wake.take());
    }
}
