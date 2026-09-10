use super::*;
use weld_client::{
    ClientFocusRequest, ClientInputEvent, ClientInputTarget, InputEventKind, KeyboardKeyState,
    LinuxKeycode,
};

fn key(session: HoistSessionId, surface: ClientSurfaceId) -> DestinationEnvelope {
    DestinationEnvelope {
        session,
        message: DestinationMessage::input(ClientInputEvent {
            target: ClientInputTarget::Keyboard { surface },
            host_position: None,
            event: InputEventKind::Keyboard {
                keycode: LinuxKeycode(30),
                state: KeyboardKeyState::Pressed,
            },
            time: 1,
        }),
    }
}

fn stable_policy() -> SchedulingPolicy {
    SchedulingPolicy {
        interaction_grace: Duration::MAX,
        starvation_age: Duration::MAX,
        ..Default::default()
    }
}

#[test]
fn source_observes_the_whole_input_batch_before_choosing_queued_work() {
    let (mut port, transport, encoder) = source_port();
    port = port.with_scheduling_policy(stable_policy());
    let source = ClientSourceId::new(1);
    let session = HoistSessionId::new(1);
    let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
    for id in 1..=3 {
        port.submit(SourcePortCommand::Surface {
            session,
            event: one_buffer_commit(
                surface(source, 1, id),
                1,
                1,
                shm_lease(source, id, id as u8, metadata),
            ),
        })
        .expect("queue");
    }
    assert_eq!(
        encoder.borrow().submitted.len(),
        1,
        "idle fast path remains immediate"
    );
    transport
        .borrow_mut()
        .incoming
        .push_back(DestinationEnvelope {
            session,
            message: DestinationMessage::Request(ClientRequest::Focus(ClientFocusRequest {
                source,
                surface: Some(surface(source, 1, 2)),
            })),
        });
    transport
        .borrow_mut()
        .incoming
        .push_back(key(session, surface(source, 1, 3)));
    let (token, frame, _) = encoder.borrow().submitted[0].clone();
    complete(&encoder, token, frame, 1);
    let messages = port.poll().expect("drain completion and input");
    assert_eq!(messages.len(), 2);
    port.submit(SourcePortCommand::Cursor {
        session,
        update: weld_client::ClientCursorUpdate {
            surface: surface(source, 1, 2),
            cursor: weld_client::ClientCursor::Hidden,
        },
        sequence: 1,
    })
    .expect("cursor ack side effect flushes output only");
    assert_eq!(encoder.borrow().submitted.len(), 1);
    for message in messages {
        port.accept_destination(&message).expect("observe");
        assert_eq!(encoder.borrow().submitted.len(), 1);
    }
    port.progress_after_destination()
        .expect("admit after every input");
    assert_eq!(encoder.borrow().submitted[1].2, vec![3, 3, 3, 255]);
    let (token, frame, _) = encoder.borrow().submitted[1].clone();
    complete(&encoder, token, frame, 1);
    assert!(port.poll().expect("no new input").is_empty());
    port.progress_after_destination()
        .expect("last background frame progresses");
    assert_eq!(encoder.borrow().submitted[2].2, vec![2, 2, 2, 255]);
}

#[test]
fn destination_input_changes_admission_not_already_decoded_publication() {
    let (mut port, transport, decoder) = destination_port();
    port = port.with_scheduling_policy(stable_policy());
    decoder.borrow_mut().capacity = Some(0);
    let session = HoistSessionId::new(1);
    let source = ClientSourceId::new(1);
    let background = surface(source, 1, 1);
    let foreground = surface(source, 1, 2);
    let frames = [
        MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(1), 0),
        MediaFrameId::new(MediaStreamId::new(2), StreamGeneration::new(1), 0),
    ];
    for (surface, frame) in [(background, frames[0]), (foreground, frames[1])] {
        transport
            .borrow_mut()
            .incoming
            .push_back(SourceTransportPacket::Control(SourceEnvelope {
                session,
                message: SourceMessage::Surface(encoded_commit(surface, 1, frame)),
            }));
        transport
            .borrow_mut()
            .incoming
            .push_back(SourceTransportPacket::Media(encoded_media(session, frame)));
    }
    port.poll().expect("busy admission");
    port.submit(DestinationPortCommand::Message(key(session, foreground)))
        .expect("input leaves immediately");
    assert_eq!(transport.borrow().sent.len(), 1);
    assert!(decoder.borrow().submitted.is_empty());
    decoder.borrow_mut().capacity = Some(1);
    port.poll().expect("capacity recovery without new frames");
    assert_eq!(decoder.borrow().submitted, vec![frames[1]]);
}

#[test]
fn same_drain_withdraw_keeps_control_order_without_starting_obsolete_decode() {
    let (mut port, transport, decoder) = destination_port();
    let session = HoistSessionId::new(1);
    let surface = surface(ClientSourceId::new(1), 1, 1);
    let frame = MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(1), 0);
    transport.borrow_mut().incoming.extend([
        SourceTransportPacket::Control(SourceEnvelope {
            session,
            message: SourceMessage::Mapped { surface },
        }),
        SourceTransportPacket::Control(SourceEnvelope {
            session,
            message: SourceMessage::Surface(encoded_commit(surface, 1, frame)),
        }),
        SourceTransportPacket::Media(encoded_media(session, frame)),
        SourceTransportPacket::Control(SourceEnvelope {
            session,
            message: SourceMessage::Withdraw { surface },
        }),
    ]);
    let records = port.poll().expect("ordered lifecycle");
    assert_eq!(records.len(), 2);
    assert!(matches!(
        records[0].event,
        DestinationPortEvent::MappedSurface(_)
    ));
    assert!(matches!(
        records[1].event,
        DestinationPortEvent::WithdrawSurface(_)
    ));
    assert!(decoder.borrow().submitted.is_empty());
}
