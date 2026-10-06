use super::*;
use weld_client::{
    ClientSurfaceRole, LogicalPoint, PopupState, SurfaceContentView, SurfaceLayerPlacement,
    SurfaceStreamMode, ToplevelHints, ToplevelState, WindowDecoration,
};

fn pixels(surface: ClientSurfaceId, revision: u64) -> ClientSurfaceEvent {
    let mut event = one_buffer_commit(
        surface,
        revision,
        1,
        shm_lease(
            surface.source(),
            revision,
            1,
            ClientBufferMetadata::new(Extent::new(100, 100), true),
        ),
    );
    let ClientSurfaceEventKind::Commit(commit) = &mut event.kind else {
        panic!("commit");
    };
    commit.make_mut().root = Some(SurfaceLayerPlacement {
        layer: SurfaceLayerId::new(1),
        position: LogicalPoint::ZERO,
        view: SurfaceContentView {
            source_x: 0.0,
            source_y: 0.0,
            source_width: 100.0,
            source_height: 100.0,
            logical_width: 100.0,
            logical_height: 100.0,
        },
    });
    event
}

#[test]
fn negotiated_composition_precedes_first_encode_and_popup_bursts_use_root_scheduler() {
    let encoder = Rc::new(RefCell::new(FakeEncoderState::default()));
    let transport = Rc::new(RefCell::new(FakeSourceTransportState {
        stream_mode: SurfaceStreamMode::Composited,
        ..Default::default()
    }));
    let mut port = EncodedSourcePort::configured(
        FakeSourceTransport(transport.clone()),
        Box::new(FakeEncoder(encoder.clone())),
        EncodedSourceOptions::default(),
    )
    .expect("port");
    let root = surface(ClientSourceId::new(1), 1, 1);
    let popup = surface(ClientSourceId::new(1), 1, 2);
    let session = HoistSessionId::new(1);
    let submit = |port: &mut EncodedSourcePort<_>, event| {
        port.submit(SourcePortCommand::Surface { session, event })
            .expect("event")
    };
    submit(
        &mut port,
        ClientSurfaceEvent {
            surface: root,
            kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                parent: None,
                decoration: WindowDecoration::ServerSide,
                hints: ToplevelHints::default(),
            })),
        },
    );
    submit(&mut port, pixels(root, 1));
    assert_eq!(
        encoder.borrow().compositions.len(),
        1,
        "first admission is already composed"
    );
    submit(
        &mut port,
        ClientSurfaceEvent {
            surface: popup,
            kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(PopupState {
                owner: root,
                position: LogicalPoint::new(10.0, 10.0),
                stack_index: 1,
            })),
        },
    );
    for revision in 2..=20 {
        submit(&mut port, pixels(popup, revision));
    }
    assert_eq!(
        encoder.borrow().compositions.len(),
        1,
        "queued scenes have not touched the GPU"
    );
    let (token, frame, _) = encoder.borrow().submitted[0].clone();
    complete(&encoder, token, frame, 1);
    port.poll().expect("complete");
    port.progress_after_destination().expect("schedule latest");
    assert_eq!(
        encoder.borrow().compositions[1],
        vec![
            ClientBufferUseId::new(root.source(), 1),
            ClientBufferUseId::new(root.source(), 20)
        ]
    );
    let (token, frame, _) = encoder.borrow().submitted[1].clone();
    complete(&encoder, token, frame, 2);
    port.poll().expect("complete");
    port.progress_after_destination().expect("idle");
    port.submit(SourcePortCommand::WithdrawSurface {
        session,
        surface: popup,
    })
    .expect("withdraw popup");
    assert_eq!(
        encoder
            .borrow()
            .compositions
            .last()
            .expect("idle-root redraw")
            .len(),
        1
    );
    assert_eq!(port.state.as_ref().expect("live").streams.len(), 1);
    assert!(!transport.borrow().disconnected);
}
