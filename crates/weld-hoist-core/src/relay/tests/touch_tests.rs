use super::*;
use weld_client::{SurfaceLayerId, TouchEvent, TouchId};

#[test]
fn relocated_touch_reaches_the_authorized_source_and_focus_loss_cancels_it() {
    let (mut source, _, surface, session) = mapped_source();
    source.effects.clear();
    let destination = ClientSourceId::new(2);
    let port = Rc::new(RefCell::new(FakeDestinationState::default()));
    let mut relay = DestinationRelayAdapter::new(
        surface.source(),
        ClientSourceDescriptor::new(destination, ClientProvenance::Relocated),
        FakeDestinationPort(port.clone()),
    );
    relay.map_surface(session, surface);
    let target = ClientInputTarget::Touch {
        surface: relocated_surface(destination, surface),
        layer: SurfaceLayerId::new(1),
    };
    for event in [
        TouchEvent::Down {
            id: TouchId(99),
            position: InputPosition::new(2.0, 3.0),
        },
        TouchEvent::Frame,
    ] {
        relay.apply_input(ClientInputEvent {
            target,
            host_position: None,
            event: InputEventKind::Touch { event },
            time: 1,
        });
        let Some(DestinationPortCommand::Message(envelope)) = port.borrow_mut().outbound.pop()
        else {
            panic!("input envelope");
        };
        assert!(source.accept_destination(envelope));
    }
    assert!(source.effects.iter().all(|effect| matches!(effect, ClientAdapterEffect::Input(input) if input.target.surface() == surface)));
    source.effects.clear();
    relay.host_focus_lost(2);
    for command in std::mem::take(&mut port.borrow_mut().outbound) {
        if let DestinationPortCommand::Message(envelope) = command {
            assert!(source.accept_destination(envelope));
        }
    }
    assert!(source.effects.iter().any(|effect| matches!(effect, ClientAdapterEffect::Input(input) if input.event == InputEventKind::Touch { event: TouchEvent::Cancel })));
    source.effects.clear();
    assert!(source.accept_destination(DestinationEnvelope {
        session,
        message: DestinationMessage::input(ClientInputEvent {
            target: ClientInputTarget::Touch {
                surface,
                layer: SurfaceLayerId::new(1)
            },
            host_position: None,
            event: InputEventKind::Touch {
                event: TouchEvent::Motion {
                    id: TouchId(99),
                    position: InputPosition::default()
                }
            },
            time: 3,
        })
    }));
    assert!(
        source.effects.is_empty(),
        "a cancelled contact cannot resume without down"
    );
}
