use super::*;
use weld_client::PresentationRate;

#[test]
fn map_cadence_popup_and_reclaim_share_one_claim_lifecycle() {
    let source = ClientSourceId::new(1);
    let root = surface(source, 1);
    let popup = surface(source, 2);
    let session = HoistSessionId::new(1);
    let state = Rc::new(RefCell::new(FakeSourceState::default()));
    let mut relay = SourceRelayAdapter::new(source, FakeSourcePort(state));
    relay.map(session, root);
    assert_eq!(
        relay.presentations[&root],
        ClientPresentationClaim::Active { rate: None }
    );
    relay.observe(&ClientSurfaceEvent {
        surface: popup,
        kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(PopupState {
            owner: root,
            position: LogicalPoint::ZERO,
            stack_index: 1,
        })),
    });
    let rate = PresentationRate::try_from(90_000).expect("rate");
    assert!(relay.accept_destination(DestinationEnvelope {
        session,
        message: DestinationMessage::Request(ClientRequest::Surface(
            weld_client::ClientSurfaceRequest {
                surface: root,
                kind: ClientSurfaceRequestKind::SetPresentation { rate: Some(rate) },
            }
        ))
    }));
    assert_eq!(
        relay.presentations[&popup],
        ClientPresentationClaim::Active { rate: Some(rate) }
    );
    assert!(
        relay.effects.is_empty(),
        "presentation must not leak as raw source request"
    );
    relay.presentation_updates.clear();
    relay.unmap(root);
    assert!(relay.presentations.is_empty());
    assert_eq!(relay.presentation_updates.len(), 2);
    assert!(
        relay
            .presentation_updates
            .iter()
            .all(|update| update.claim == ClientPresentationClaim::Release)
    );
}

#[test]
fn transport_failure_releases_claim_and_late_requests_cannot_reactivate_it() {
    let source = ClientSourceId::new(1);
    let root = surface(source, 1);
    let state = Rc::new(RefCell::new(FakeSourceState::default()));
    let mut relay = SourceRelayAdapter::new(source, FakeSourcePort(state));
    relay.map(HoistSessionId::new(1), root);
    relay.presentation_updates.clear();
    relay.fail("disconnected");
    relay.set_presentation(root, ClientPresentationClaim::Active { rate: None });
    assert_eq!(
        relay.presentation_updates,
        vec![ClientPresentationUpdate {
            surface: root,
            claim: ClientPresentationClaim::Release
        }]
    );
    assert!(relay.presentations.is_empty());
}
