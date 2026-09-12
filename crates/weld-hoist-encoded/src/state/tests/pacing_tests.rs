use super::*;
use weld_client::{ClientPresentationClaim, PresentationRate};

fn active() -> ClientPresentationClaim {
    ClientPresentationClaim::Active {
        rate: Some(PresentationRate::HZ_60),
    }
}

fn pixels(surface: ClientSurfaceId, revision: u64, value: u8) -> ClientSurfaceEvent {
    one_buffer_commit(
        surface,
        revision,
        1,
        shm_lease(
            surface.source(),
            revision,
            value,
            ClientBufferMetadata::new(Extent::new(1, 1), true),
        ),
    )
}

#[test]
fn fast_and_queued_paths_coalesce_to_the_final_snapshot_at_the_deadline() {
    let (mut port, _, encoder) = source_port();
    let surface = surface(ClientSourceId::new(1), 1, 1);
    let session = HoistSessionId::new(1);
    port.set_presentation(surface, active()).expect("claim");
    port.submit(SourcePortCommand::Surface {
        session,
        event: pixels(surface, 1, 1),
    })
    .expect("first");
    let (token, frame, _) = encoder.borrow().submitted[0].clone();
    complete(&encoder, token, frame, 1);
    port.poll().expect("complete");
    port.progress_after_destination().expect("progress");
    assert!(port.next_deadline().is_none(), "idle is not a repeat timer");
    // Put the next slot well into the future so this is independent of test CPU speed.
    let start = Instant::now() + Duration::from_secs(10);
    port.state
        .as_mut()
        .expect("state")
        .pacing
        .admitted(surface, start);
    for revision in 2..=120 {
        port.submit(SourcePortCommand::Surface {
            session,
            event: pixels(surface, revision, revision as u8),
        })
        .expect("coalesced");
    }
    assert_eq!(encoder.borrow().submitted.len(), 1);
    let state = port.state.as_mut().expect("state");
    assert_eq!(state.pending[&surface].len(), 1);
    let deadline = state.next_deadline().expect("final frame wake");
    state
        .schedule_at(deadline - Duration::from_nanos(1))
        .expect("not due");
    assert_eq!(encoder.borrow().submitted.len(), 1);
    state
        .schedule_at(deadline)
        .expect("due without another event");
    assert_eq!(encoder.borrow().submitted.len(), 2);
    assert_eq!(encoder.borrow().submitted[1].2, vec![120, 120, 120, 255]);
    assert!(
        state.next_deadline().is_none(),
        "completion wake owns in-flight work"
    );
    state.schedule_at(deadline).expect("same time");
    assert_eq!(
        encoder.borrow().submitted.len(),
        2,
        "no immediate catch-up loop"
    );
}

#[test]
fn deadline_is_suppressed_for_other_blockers_and_overdue_work_progresses_once() {
    let (mut state, encoder) = source();
    let surface = surface(ClientSourceId::new(1), 1, 1);
    let session = HoistSessionId::new(1);
    state.activity.register(session, surface);
    state.pacing.set(surface, active());
    let past = Instant::now() - Duration::from_secs(1);
    state.pacing.admitted(surface, past);
    state
        .queue_event(session, pixels(surface, 1, 1))
        .expect("pending");
    let due = state
        .next_deadline()
        .expect("overdue deadline must not be lost");
    assert!(due < Instant::now());
    state.admission_deferred = true;
    assert!(state.next_deadline().is_none());
    state.admission_deferred = false;
    state.transport_blocked = true;
    assert!(state.next_deadline().is_none());
    state.transport_blocked = false;
    state.resizing.insert(surface);
    assert!(state.next_deadline().is_none());
    state.resizing.remove(&surface);
    state.pacing.set(surface, ClientPresentationClaim::Paused);
    assert!(state.next_deadline().is_none());
    state.pacing.set(surface, active());
    state.schedule().expect("overdue work");
    state.schedule().expect("repeat service");
    assert_eq!(encoder.borrow().submitted.len(), 1);
    assert!(state.next_deadline().is_none());
}

#[test]
fn paused_latest_lease_is_retained_for_resume_and_dropped_on_disconnect() {
    let (mut port, _, encoder) = source_port();
    let surface = surface(ClientSourceId::new(1), 1, 1);
    let session = HoistSessionId::new(1);
    port.set_presentation(surface, ClientPresentationClaim::Paused)
        .expect("pause");
    let released = Rc::new(std::cell::Cell::new(0));
    for revision in 1..=20 {
        let release = released.clone();
        let lease = ClientBufferLease::new(
            ClientBufferId::new(surface.source(), revision),
            ClientBufferUseId::new(surface.source(), revision),
            ClientBufferMetadata::new(Extent::new(1, 1), true),
            Rc::new(vec![revision as u8; 4]),
            move |_| release.set(release.get() + 1),
        )
        .expect("lease");
        port.submit(SourcePortCommand::Surface {
            session,
            event: one_buffer_commit(surface, revision, 1, lease),
        })
        .expect("paused snapshot");
    }
    assert!(encoder.borrow().submitted.is_empty());
    assert_eq!(
        released.get(),
        19,
        "only latest source pixels remain pinned"
    );
    assert!(port.next_deadline().is_none());
    port.set_presentation(surface, active()).expect("resume");
    port.progress_after_destination().expect("resume latest");
    assert_eq!(encoder.borrow().submitted[0].2, vec![20; 4]);
    assert_eq!(
        released.get(),
        20,
        "fake copied input does not borrow source storage"
    );
    port.set_presentation(surface, ClientPresentationClaim::Paused)
        .expect("pause again");
    let release = released.clone();
    let lease = ClientBufferLease::new(
        ClientBufferId::new(surface.source(), 21),
        ClientBufferUseId::new(surface.source(), 21),
        ClientBufferMetadata::new(Extent::new(1, 1), true),
        Rc::new(vec![21; 4]),
        move |_| release.set(release.get() + 1),
    )
    .expect("lease");
    port.submit(SourcePortCommand::Surface {
        session,
        event: one_buffer_commit(surface, 21, 1, lease),
    })
    .expect("last");
    port.disconnect();
    assert_eq!(released.get(), 21);
}

#[test]
fn full_unmap_preserves_control_order_and_cancels_late_mapped_completion() {
    let (mut port, transport, encoder) = source_port();
    let surface = surface(ClientSourceId::new(1), 1, 1);
    let session = HoistSessionId::new(1);
    port.set_presentation(surface, active()).expect("claim");
    port.submit(SourcePortCommand::Surface {
        session,
        event: pixels(surface, 1, 1),
    })
    .expect("in flight");
    let (token, frame, _) = encoder.borrow().submitted[0].clone();
    port.set_presentation(surface, ClientPresentationClaim::Paused)
        .expect("pause");
    port.submit(SourcePortCommand::Surface {
        session,
        event: pixels(surface, 2, 2),
    })
    .expect("pending");
    port.submit(SourcePortCommand::Surface {
        session,
        event: ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Interaction(ToplevelInteractionRequestKind::Move),
        },
    })
    .expect("control barrier");
    port.submit(SourcePortCommand::Surface {
        session,
        event: pixels(surface, 3, 3),
    })
    .expect("after barrier");
    let mut unmap = commit(surface, 4, Vec::new());
    if let ClientSurfaceEventKind::Commit(commit) = &mut unmap.kind {
        commit.mapped = false;
    }
    port.submit(SourcePortCommand::Surface {
        session,
        event: unmap,
    })
    .expect("unmap immediately");
    let packets = std::mem::take(&mut transport.borrow_mut().sent);
    assert_eq!(packets.len(), 2);
    assert!(matches!(
        &packets[0],
        SourceTransportPacket::Control(SourceEnvelope {
            message: SourceMessage::Surface(WireClientSurfaceEvent {
                kind: WireClientSurfaceEventKind::Interaction(_),
                ..
            }),
            ..
        })
    ));
    assert!(
        matches!(&packets[1],SourceTransportPacket::Control(SourceEnvelope {message:SourceMessage::Surface(WireClientSurfaceEvent {kind:WireClientSurfaceEventKind::Commit(commit),..}),..}) if !commit.mapped)
    );
    complete(&encoder, token, frame, 1);
    port.poll().expect("cancelled completion");
    port.progress_after_destination().expect("idle");
    assert!(
        transport.borrow().sent.is_empty(),
        "late completion must not remap the window"
    );
    assert!(port.next_deadline().is_none());
    assert_eq!(encoder.borrow().submitted.len(), 1);
}

#[test]
fn paced_inventory_merge_keeps_unobserved_retained_layers() {
    let (mut port, _, encoder) = source_port();
    let surface = surface(ClientSourceId::new(1), 1, 1);
    let session = HoistSessionId::new(1);
    let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
    port.set_presentation(surface, ClientPresentationClaim::Paused)
        .expect("pause");
    let replaced = |layer, value| SurfaceBufferUpdate {
        layer: SurfaceLayerId::new(layer),
        change: SurfaceBufferChange::Replaced {
            metadata,
            buffer: shm_lease(surface.source(), layer, value, metadata),
        },
    };
    port.submit(SourcePortCommand::Surface {
        session,
        event: commit(surface, 1, vec![replaced(1, 11), replaced(2, 12)]),
    })
    .expect("initial layers");
    port.submit(SourcePortCommand::Surface {
        session,
        event: commit(
            surface,
            2,
            vec![
                SurfaceBufferUpdate {
                    layer: SurfaceLayerId::new(1),
                    change: SurfaceBufferChange::Retained { metadata },
                },
                replaced(2, 22),
            ],
        ),
    })
    .expect("newest inventory");
    port.set_presentation(surface, active()).expect("resume");
    port.progress_after_destination().expect("first layer");
    assert_eq!(encoder.borrow().submitted[0].2, vec![11, 11, 11, 255]);
    let (token, frame, _) = encoder.borrow().submitted[0].clone();
    complete(&encoder, token, frame, 1);
    port.poll().expect("next layer shares the snapshot slot");
    assert_eq!(encoder.borrow().submitted[1].2, vec![22, 22, 22, 255]);
}

#[test]
fn a_paced_surface_does_not_block_another_surfaces_first_frame() {
    let (mut state, encoder) = source();
    let first = surface(ClientSourceId::new(1), 1, 1);
    let second = surface(ClientSourceId::new(1), 1, 2);
    let session = HoistSessionId::new(1);
    let now = Instant::now();
    state.pacing.set(first, active());
    state.pacing.admitted(first, now);
    state.activity.register(session, first);
    state.activity.register(session, second);
    state
        .queue_event(session, pixels(first, 1, 1))
        .expect("first");
    state
        .queue_event(session, pixels(second, 2, 2))
        .expect("second");
    state.schedule_at(now).expect("select eligible surface");
    assert_eq!(encoder.borrow().submitted[0].2, vec![2, 2, 2, 255]);
    assert_eq!(state.pending[&first].len(), 1);
}
