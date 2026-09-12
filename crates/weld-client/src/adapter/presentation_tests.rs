use super::*;
use crate::{ClientId, ClientPresentationClaim, ClientPresentationUpdate, ClientProvenance};
use std::{cell::RefCell, rc::Rc};

#[derive(Default)]
struct Record {
    updates: Vec<ClientPresentationUpdate>,
    received: Vec<(ClientSourceId, ClientPresentationUpdate)>,
    effect_drains: usize,
}

struct Adapter {
    scope: Option<ClientSourceId>,
    record: Rc<RefCell<Record>>,
}
impl ClientAdapter for Adapter {
    fn drain_events(&mut self, _: &mut ClientEventQueue) {}
    fn apply_input(&mut self, _: ClientInputEvent) {}
    fn apply_request(&mut self, _: ClientRequest) {}
    fn host_focus_lost(&mut self, _: u32) {}
    fn apply_command(&mut self, command: ClientAdapterCommandEnvelope) {
        if let Ok(update) = command.downcast::<ClientPresentationUpdate>() {
            self.record.borrow_mut().updates.push(*update);
        }
    }
    fn presentation_source(&self) -> Option<ClientSourceId> {
        self.scope
    }
    fn drain_presentation_claims(&mut self, updates: &mut Vec<ClientPresentationUpdate>) {
        updates.append(&mut self.record.borrow_mut().updates);
    }
    fn apply_presentation(&mut self, owner: ClientSourceId, update: ClientPresentationUpdate) {
        self.record.borrow_mut().received.push((owner, update));
    }
    fn drain_effects(&mut self, _: &mut Vec<ClientAdapterEffect>) {
        self.record.borrow_mut().effect_drains += 1;
    }
}

#[test]
fn presentation_commands_are_scoped_and_route_before_another_event_drain() {
    let mut runtime = ClientRuntime::default();
    let records = (0..4)
        .map(|_| Rc::new(RefCell::new(Record::default())))
        .collect::<Vec<_>>();
    for (id, record) in records.iter().enumerate() {
        runtime
            .register(ClientRuntimeAdapter::new(
                ClientSourceDescriptor::new(
                    ClientSourceId::new(id as u64),
                    if id == 0 {
                        ClientProvenance::Local
                    } else {
                        ClientProvenance::Relocated
                    },
                ),
                Adapter {
                    scope: (id != 3).then_some(ClientSourceId::new(0)),
                    record: record.clone(),
                },
            ))
            .expect("register");
    }
    let surface = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 1), 1);
    let update = ClientPresentationUpdate {
        surface,
        claim: ClientPresentationClaim::Active { rate: None },
    };
    for owner in [1, 2, 3] {
        assert!(runtime.apply_command(ClientAdapterCommandEnvelope::new(
            ClientSourceId::new(owner),
            update
        )));
    }
    let mut invalid = Vec::new();
    runtime.apply_pending_presentations(&mut invalid);
    assert_eq!(invalid.len(), 1, "undeclared upstream rejected");
    assert_eq!(
        records[0].borrow().received,
        vec![
            (ClientSourceId::new(1), update),
            (ClientSourceId::new(2), update)
        ]
    );
    assert!(
        records
            .iter()
            .all(|record| record.borrow().effect_drains == 0)
    );
    let wrong = ClientPresentationUpdate {
        surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(2), 1), 1),
        ..update
    };
    runtime.apply_command(ClientAdapterCommandEnvelope::new(
        ClientSourceId::new(1),
        wrong,
    ));
    runtime.apply_pending_presentations(&mut invalid);
    assert_eq!(
        invalid.len(),
        2,
        "claim cannot target another or relocated source"
    );
    assert!(records[2].borrow().received.is_empty());
}
