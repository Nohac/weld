//! Bounded touch validation and teardown at both relay boundaries.
use std::collections::HashMap;
use weld_client::{ClientInputTarget, ClientSurfaceId, MAX_TOUCH_CONTACTS, TouchEvent, TouchId};

#[derive(Default)]
pub(super) struct TouchLedger {
    contacts: HashMap<TouchId, ClientInputTarget>,
    pending: Vec<ClientInputTarget>,
}

impl TouchLedger {
    pub fn accepts(&self, target: ClientInputTarget, event: TouchEvent) -> bool {
        if !matches!(target, ClientInputTarget::Touch { .. }) || !event.is_valid() {
            return false;
        }
        match event {
            TouchEvent::Down { id, .. } => {
                !self.contacts.contains_key(&id)
                    && self.contacts.len() < MAX_TOUCH_CONTACTS
                    && (self.pending.contains(&target) || self.pending.len() < MAX_TOUCH_CONTACTS)
            }
            TouchEvent::Motion { id, .. } | TouchEvent::Up { id } => {
                self.contacts.get(&id) == Some(&target)
            }
            TouchEvent::Frame => self.pending.contains(&target),
            TouchEvent::Cancel => {
                self.pending.contains(&target)
                    || self.contacts.values().any(|current| *current == target)
            }
        }
    }
    pub fn observe(&mut self, target: ClientInputTarget, event: TouchEvent) -> bool {
        if !self.accepts(target, event) {
            return false;
        }
        match event {
            TouchEvent::Down { id, .. } => {
                self.contacts.insert(id, target);
            }
            TouchEvent::Up { id } => {
                self.contacts.remove(&id);
            }
            TouchEvent::Cancel => {
                self.contacts.retain(|_, current| *current != target);
            }
            _ => {}
        }
        if matches!(event, TouchEvent::Frame | TouchEvent::Cancel) {
            self.pending.retain(|current| *current != target);
        } else if !self.pending.contains(&target) {
            self.pending.push(target);
        }
        true
    }
    pub fn retire(&mut self, matches: impl Fn(ClientSurfaceId) -> bool) -> Vec<ClientInputTarget> {
        let mut targets = Vec::new();
        for target in self
            .contacts
            .values()
            .copied()
            .chain(self.pending.iter().copied())
        {
            if matches(target.surface()) && !targets.contains(&target) {
                targets.push(target);
            }
        }
        self.contacts.retain(|_, target| !targets.contains(target));
        self.pending.retain(|target| !targets.contains(target));
        targets
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_client::{ClientId, ClientSourceId, InputPosition, SurfaceLayerId};

    #[test]
    fn contact_identity_cannot_cross_targets_and_admission_is_bounded() {
        let target = ClientInputTarget::Touch {
            surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 1), 1),
            layer: SurfaceLayerId::new(1),
        };
        let other = ClientInputTarget::Touch {
            surface: target.surface(),
            layer: SurfaceLayerId::new(2),
        };
        let mut ledger = TouchLedger::default();
        for id in 0..MAX_TOUCH_CONTACTS {
            assert!(ledger.observe(
                target,
                TouchEvent::Down {
                    id: TouchId(id as u64),
                    position: InputPosition::default()
                }
            ));
        }
        assert!(!ledger.observe(
            target,
            TouchEvent::Down {
                id: TouchId(99),
                position: InputPosition::default()
            }
        ));
        assert!(!ledger.observe(other, TouchEvent::Up { id: TouchId(0) }));
        assert!(!ledger.observe(
            target,
            TouchEvent::Motion {
                id: TouchId(0),
                position: InputPosition::new(f64::INFINITY, 0.0)
            }
        ));
        assert!(ledger.observe(target, TouchEvent::Frame));
        assert!(!ledger.observe(target, TouchEvent::Frame));
        assert_eq!(
            ledger.retire(|surface| surface == target.surface()),
            [target]
        );
        assert!(!ledger.observe(target, TouchEvent::Up { id: TouchId(0) }));
    }
}
