use super::*;
use weld_client::PresentationRate;

#[test]
fn bitrate_hints_are_authorized_then_consumed_without_native_effects() {
    let source = ClientSourceId::new(1);
    let root = surface(source, 1);
    let session = HoistSessionId::new(1);
    let state = Rc::new(RefCell::new(FakeSourceState::default()));
    let mut relay = SourceRelayAdapter::new(source, FakeSourcePort(state.clone()));
    relay.map(session, root);
    let hint = |session| DestinationEnvelope {
        session,
        message: DestinationMessage::Request(ClientRequest::Surface(
            weld_client::ClientSurfaceRequest {
                surface: root,
                kind: ClientSurfaceRequestKind::SetBitratePreference { preference: None },
            },
        )),
    };
    assert!(relay.accept_destination(hint(session)));
    assert_eq!(state.borrow().accepted, 1);
    assert!(relay.effects.is_empty());
    assert!(!relay.accept_destination(hint(HoistSessionId::new(2))));
    assert_eq!(
        state.borrow().accepted,
        1,
        "cross-session hint never reaches the port"
    );

    let state = Rc::new(RefCell::new(FakeSourceState::default()));
    let mut relay = SourceRelayAdapter::new(source, FakeSourcePort(state.clone()));
    assert!(relay.accept_destination(hint(session)));
    assert_eq!(
        state.borrow().accepted,
        0,
        "stale surface cannot receive preferences"
    );
}

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

#[test]
fn accepted_callback_rate_keeps_requested_popup_preference_and_deadline_forwarding() {
    let source = ClientSourceId::new(1);
    let root = surface(source, 1);
    let popup = surface(source, 2);
    let session = HoistSessionId::new(1);
    let deadline = Instant::now();
    let state = Rc::new(RefCell::new(FakeSourceState {
        ceiling: Some(PresentationRate::HZ_60),
        deadline: Some(deadline),
        ..Default::default()
    }));
    let mut relay = SourceRelayAdapter::new(source, FakeSourcePort(state.clone()));
    relay.map(session, root);
    relay.presentation_updates.clear();
    let requested = ClientPresentationClaim::Active {
        rate: Some(PresentationRate::try_from(120_000).expect("rate")),
    };
    relay.set_presentation(root, requested);
    relay.observe(&ClientSurfaceEvent {
        surface: popup,
        kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(PopupState {
            owner: root,
            position: LogicalPoint::ZERO,
            stack_index: 1,
        })),
    });
    assert_eq!(relay.presentations[&root], requested);
    assert_eq!(relay.presentations[&popup], requested);
    assert!(relay.presentation_updates.iter().all(|update| update.claim
        == ClientPresentationClaim::Active {
            rate: Some(PresentationRate::HZ_60)
        }));
    assert!(state.borrow().presentations.contains(&(popup, requested)));
    assert_eq!(relay.next_deadline(), Some(deadline));
    relay.fail("disconnected");
    assert!(relay.next_deadline().is_none());
}
