//! Receiver scheduling contracts with no graphics driver or native importer.

use super::*;

fn frame(stream: u64, generation: u64, sequence: u64) -> MediaFrameId {
    MediaFrameId::new(
        MediaStreamId::new(stream),
        StreamGeneration::new(generation),
        sequence,
    )
}

pub(super) fn tree(
    surface: ClientSurfaceId,
    revision: u64,
    frames: &[MediaFrameId],
) -> WireClientSurfaceEvent<EncodedBuffer> {
    let mut event = encoded_commit(surface, revision, frames[0]);
    let WireClientSurfaceEventKind::Commit(commit) = &mut event.kind else {
        panic!("commit");
    };
    commit.buffers = frames
        .iter()
        .enumerate()
        .map(|(index, frame)| weld_client::WireSurfaceBufferUpdate {
            layer: SurfaceLayerId::new(index as u64 + 1),
            change: WireSurfaceBufferChange::Replaced {
                metadata: ClientBufferMetadata::new(Extent::new(1, 1), true),
                buffer: EncodedBuffer { frame: *frame },
            },
        })
        .collect();
    event
}

fn queue(
    transport: &Rc<RefCell<FakeDestinationTransportState>>,
    surface: ClientSurfaceId,
    revision: u64,
    frames: &[MediaFrameId],
) {
    let session = HoistSessionId::new(1);
    let mut transport = transport.borrow_mut();
    transport
        .incoming
        .push_back(SourceTransportPacket::Control(SourceEnvelope {
            session,
            message: SourceMessage::Surface(tree(surface, revision, frames)),
        }));
    transport.incoming.extend(
        frames
            .iter()
            .map(|frame| SourceTransportPacket::Media(encoded_media(session, *frame))),
    );
}

fn complete_decode(decoder: &Rc<RefCell<FakeDecoderState>>, index: usize) {
    let mut decoder = decoder.borrow_mut();
    let frame = decoder.submitted[index];
    let token = decoder.tokens[index];
    decoder.completions.push(DecodeCompletion {
        token,
        timing: None,
        result: Ok(vec![DecodedFrame {
            frame,
            buffer: Extent::new(1, 1),
        }]),
    });
}

#[test]
fn one_four_layer_commit_fills_four_slots_and_applies_atomically() {
    let (mut port, transport, decoder) = destination_port();
    decoder.borrow_mut().capacity = Some(4);
    port.state.as_mut().expect("state").publisher.enabled = true;
    let surface = surface(ClientSourceId::new(1), 1, 1);
    let frames = (1..=4)
        .map(|stream| frame(stream, 1, 0))
        .collect::<Vec<_>>();
    queue(&transport, surface, 1, &frames);
    assert!(port.poll().expect("submit all layers").is_empty());
    assert_eq!(decoder.borrow().submitted, frames);
    assert_eq!(
        port.state.as_ref().expect("state").decode_in_flight.len(),
        4
    );
    for index in [3, 1, 0] {
        complete_decode(&decoder, index);
        assert!(port.poll().expect("partial completion").is_empty());
    }
    complete_decode(&decoder, 2);
    let records = port.poll().expect("atomic application");
    assert_eq!(records.len(), 1);
    assert!(
        matches!(&records[0].event, DestinationPortEvent::Surface(ClientSurfaceEvent {
        kind: ClientSurfaceEventKind::Commit(commit), ..
    }) if commit.buffers.len() == 4)
    );
    assert!(
        port.state
            .as_ref()
            .expect("state")
            .decode_in_flight
            .is_empty()
    );
}

#[test]
fn three_backlogged_surfaces_get_fair_turns_without_advancing_their_commit_order() {
    let (mut port, transport, decoder) = destination_port();
    decoder.borrow_mut().capacity = Some(0);
    port.state.as_mut().expect("state").publisher.enabled = true;
    for sequence in 0..2 {
        for stream in 1..=3 {
            queue(
                &transport,
                surface(ClientSourceId::new(1), 1, stream),
                sequence + 1,
                &[frame(stream, 1, sequence)],
            );
        }
    }
    port.poll()
        .expect("all surfaces backlogged before capacity recovers");
    assert!(decoder.borrow().submitted.is_empty());
    decoder.borrow_mut().capacity = Some(1);
    port.poll().expect("initial submission");
    for index in 0..6 {
        assert_eq!(decoder.borrow().submitted.len(), index + 1);
        complete_decode(&decoder, index);
        port.poll().expect("next fair turn");
    }
    assert_eq!(
        decoder.borrow().submitted,
        vec![
            frame(1, 1, 0),
            frame(2, 1, 0),
            frame(3, 1, 0),
            frame(1, 1, 1),
            frame(2, 1, 1),
            frame(3, 1, 1)
        ]
    );
}

#[test]
fn busy_preserves_payload_allocation_ingress_time_and_last_pending_work() {
    let (mut port, transport, decoder) = destination_port();
    port.state.as_mut().expect("state").publisher.enabled = true;
    let surface = surface(ClientSourceId::new(1), 1, 1);
    let frames = [frame(1, 1, 0), frame(2, 1, 0)];
    queue(&transport, surface, 1, &frames);
    port.poll().expect("one slot busy");
    let state = port.state.as_ref().expect("state");
    let pending = state.media_frames.get(&frames[1]).expect("pending");
    let pointer = pending.access_unit.payload.as_ptr();
    let received_at = pending.received_at;
    for _ in 0..3 {
        port.poll().expect("still busy");
    }
    let pending = port
        .state
        .as_ref()
        .expect("state")
        .media_frames
        .get(&frames[1])
        .expect("pending");
    assert_eq!(pending.access_unit.payload.as_ptr(), pointer);
    assert_eq!(pending.received_at, received_at);
    complete_decode(&decoder, 0);
    port.poll().expect("capacity wake without new packets");
    assert_eq!(decoder.borrow().submitted, frames);
}

#[test]
fn obsolete_context_retires_while_its_completed_output_waits_for_atomic_application() {
    let (mut port, transport, decoder) = destination_port();
    port.state.as_mut().expect("state").publisher.enabled = true;
    let surface = surface(ClientSourceId::new(1), 1, 1);
    let old = [frame(1, 1, 0), frame(2, 1, 0)];
    let new = [frame(1, 2, 0), frame(2, 2, 0)];
    queue(&transport, surface, 1, &old);
    queue(&transport, surface, 2, &new);
    port.poll().expect("old front only");
    assert_eq!(decoder.borrow().submitted, vec![old[0]]);
    complete_decode(&decoder, 0);
    assert!(port.poll().expect("old output held").is_empty());
    assert!(
        port.state
            .as_ref()
            .expect("state")
            .decoded
            .contains_key(&old[0])
    );
    assert!(
        decoder
            .borrow()
            .retirements
            .contains(&(old[0].stream, old[0].generation))
    );
    assert_eq!(decoder.borrow().submitted, old);
    complete_decode(&decoder, 1);
    assert_eq!(port.poll().expect("first atomic frame").len(), 1);
    assert_eq!(decoder.borrow().submitted[2], new[0]);
}

#[test]
fn cancelling_several_jobs_waits_for_each_completion_without_resurrection() {
    let (mut port, transport, decoder) = destination_port();
    decoder.borrow_mut().capacity = Some(4);
    let surface = surface(ClientSourceId::new(1), 1, 1);
    let frames = (1..=4)
        .map(|stream| frame(stream, 1, 0))
        .collect::<Vec<_>>();
    queue(&transport, surface, 1, &frames);
    port.poll().expect("four jobs");
    port.state
        .as_mut()
        .expect("state")
        .cancel_surface(surface)
        .expect("cancel");
    assert!(decoder.borrow().retirements.is_empty());
    for index in [2, 0, 3, 1] {
        complete_decode(&decoder, index);
        assert!(port.poll().expect("cancelled result").is_empty());
    }
    assert_eq!(decoder.borrow().retirements.len(), 4);
    assert!(port.state.as_ref().expect("state").decoded.is_empty());
    assert!(
        port.state
            .as_ref()
            .expect("state")
            .decode_in_flight
            .is_empty()
    );
}

#[test]
fn terminal_failure_does_not_hide_other_completions_in_the_same_batch() {
    let (mut port, transport, decoder) = destination_port();
    decoder.borrow_mut().capacity = Some(2);
    let surface = surface(ClientSourceId::new(1), 1, 1);
    queue(&transport, surface, 1, &[frame(1, 1, 0), frame(2, 1, 0)]);
    port.poll().expect("two jobs");
    complete_decode(&decoder, 1);
    let token = decoder.borrow().tokens[0];
    decoder.borrow_mut().completions.push(DecodeCompletion {
        token,
        timing: None,
        result: Err(anyhow::anyhow!("failed worker job")),
    });
    decoder.borrow_mut().terminal_failure = Some(anyhow::anyhow!("original device failure"));
    assert!(
        port.poll()
            .err()
            .expect("terminal error")
            .to_string()
            .contains("original device failure")
    );
    let state = port.state.as_ref().expect("state");
    assert!(state.decode_in_flight.is_empty());
    assert!(state.decoded.contains_key(&frame(2, 1, 0)));
}

#[test]
fn sixteen_streams_rotate_while_backlogged_without_exceeding_context_budget() {
    let (mut port, transport, decoder) = destination_port();
    {
        let mut decoder = decoder.borrow_mut();
        decoder.capacity = Some(4);
        decoder.generation_limit = Some(16);
    }
    port.state.as_mut().expect("state").publisher.enabled = true;
    let surface = surface(ClientSourceId::new(1), 1, 1);
    for (revision, generation, sequence) in [(1, 1, 0), (2, 1, 1), (3, 2, 0)] {
        let frames = (1..=16)
            .map(|stream| frame(stream, generation, sequence))
            .collect::<Vec<_>>();
        queue(&transport, surface, revision, &frames);
    }
    port.poll().expect("initial jobs");
    let mut completed = 0;
    let mut applied = 0;
    while completed < 48 {
        let submitted = decoder.borrow().submitted.len();
        assert!(
            submitted > completed,
            "context budget must not wedge a partial atomic commit"
        );
        for index in (completed..submitted).rev() {
            complete_decode(&decoder, index);
        }
        completed = submitted;
        applied += port.poll().expect("backlog progress").len();
        assert!(decoder.borrow().generations.len() <= 16);
        assert!(port.state.as_ref().expect("state").decode_in_flight.len() <= 4);
    }
    assert_eq!(applied, 3);
    assert_eq!(decoder.borrow().retirements.len(), 16);
    assert!(port.state.as_ref().expect("state").decoded.is_empty());
}

#[test]
fn local_worker_timing_separates_residence_and_pipeline_stages_from_host_handoff() {
    let (mut port, transport, decoder) = destination_port();
    port.state.as_mut().expect("state").publisher.enabled = true;
    queue(
        &transport,
        surface(ClientSourceId::new(1), 1, 1),
        1,
        &[frame(1, 1, 0)],
    );
    port.poll().expect("submit");
    let start = Instant::now() - Duration::from_millis(100);
    port.state
        .as_mut()
        .expect("state")
        .decode_in_flight
        .values_mut()
        .next()
        .expect("job")
        .submitted_at = start;
    complete_decode(&decoder, 0);
    decoder.borrow_mut().completions[0].timing = Some(weld_media::DecodeTiming {
        queued_at: start + Duration::from_millis(1),
        started_at: start + Duration::from_millis(2),
        completed_at: start + Duration::from_millis(5),
        pipeline: Some(weld_media::DecodePipelineTiming {
            submitted_at: start + Duration::from_millis(3),
            finishing_at: start + Duration::from_millis(4),
            had_pending_frame: true,
        }),
    });
    port.poll().expect("apply");
    let report = port
        .state
        .as_mut()
        .expect("state")
        .observations
        .take_report(Instant::now(), DestinationGauges::default(), true)
        .expect("report");
    assert_eq!(report.counters.worker_queue.total, Duration::from_millis(1));
    assert_eq!(
        report.counters.worker_residence.total,
        Duration::from_millis(3)
    );
    assert!(report.counters.completion_handoff.total >= Duration::from_millis(95));
    assert!(report.counters.decode_wall.total >= Duration::from_millis(100));
    assert_eq!(
        report.counters.worker_submission.total,
        Duration::from_millis(1)
    );
    assert_eq!(
        report.counters.worker_pending.total,
        Duration::from_millis(1)
    );
    assert_eq!(
        report.counters.worker_finish.total,
        Duration::from_millis(1)
    );
    assert_eq!(report.counters.overlapped_submissions, 1);
}

#[test]
fn deferred_retirement_ack_releases_busy_request_without_new_transport_input() {
    let (mut port, transport, decoder) = destination_port();
    {
        let mut decoder = decoder.borrow_mut();
        decoder.generation_limit = Some(1);
        decoder.defer_retirement = true;
    }
    port.state.as_mut().expect("state").publisher.enabled = true;
    let surface = surface(ClientSourceId::new(1), 1, 1);
    let old = frame(1, 1, 0);
    let new = frame(1, 2, 0);
    queue(&transport, surface, 1, &[old]);
    queue(&transport, surface, 2, &[new]);
    port.poll().expect("old generation submitted");
    complete_decode(&decoder, 0);
    assert_eq!(port.poll().expect("old output applied").len(), 1);
    assert_eq!(decoder.borrow().submitted, vec![old]);
    assert_eq!(decoder.borrow().retirement_acks.len(), 1);
    assert!(
        decoder
            .borrow()
            .generations
            .contains(&(old.stream, old.generation))
    );
    let pending = &port.state.as_ref().expect("state").media_frames[&new];
    let pointer = pending.access_unit.payload.as_ptr();
    let received_at = pending.received_at;
    port.state
        .as_mut()
        .expect("state")
        .advance(&mut Vec::new())
        .expect("still Busy before ACK drain");
    let pending = &port.state.as_ref().expect("state").media_frames[&new];
    assert_eq!(pending.access_unit.payload.as_ptr(), pointer);
    assert_eq!(pending.received_at, received_at);
    assert!(transport.borrow().incoming.is_empty());
    port.poll().expect("retirement ACK permits new generation");
    assert_eq!(decoder.borrow().submitted, vec![old, new]);
    complete_decode(&decoder, 1);
    assert_eq!(port.poll().expect("new output applied").len(), 1);
}

#[test]
fn destroyed_or_withdrawn_surface_can_reenter_ready_queue_exactly_once() {
    for destroyed in [true, false] {
        let (mut port, transport, decoder) = destination_port();
        decoder.borrow_mut().capacity = Some(0);
        port.state.as_mut().expect("state").publisher.enabled = true;
        let surface = surface(ClientSourceId::new(1), 1, 1);
        queue(&transport, surface, 1, &[frame(1, 1, 0)]);
        port.poll().expect("first queued surface");
        assert_eq!(port.state.as_ref().expect("state").ready_surfaces.len(), 1);
        let message = if destroyed {
            SourceMessage::Surface(WireClientSurfaceEvent {
                surface,
                kind: WireClientSurfaceEventKind::Destroyed,
            })
        } else {
            SourceMessage::Withdraw { surface }
        };
        transport
            .borrow_mut()
            .incoming
            .push_back(SourceTransportPacket::Control(SourceEnvelope {
                session: HoistSessionId::new(1),
                message,
            }));
        assert_eq!(port.poll().expect("cancel queued surface").len(), 1);
        assert!(
            port.state
                .as_ref()
                .expect("state")
                .ready_surfaces
                .is_empty()
        );
        queue(&transport, surface, 2, &[frame(2, 1, 0)]);
        port.poll().expect("same surface queued again");
        assert_eq!(
            port.state.as_ref().expect("state").ready_surfaces,
            VecDeque::from([surface])
        );
        decoder.borrow_mut().capacity = Some(1);
        port.poll().expect("resume recreated surface");
        complete_decode(&decoder, 0);
        assert_eq!(port.poll().expect("recreated surface applied").len(), 1);
        assert!(
            port.state
                .as_ref()
                .expect("state")
                .ready_surfaces
                .is_empty()
        );
    }
}

#[test]
fn lookahead_is_one_commit_and_out_of_order_results_still_apply_in_order() {
    let (mut port, transport, decoder) = destination_port();
    decoder.borrow_mut().capacity = Some(8);
    port.state.as_mut().expect("state").publisher.enabled = true;
    let surface = surface(ClientSourceId::new(1), 1, 1);
    for sequence in 0..3 {
        queue(&transport, surface, sequence + 1, &[frame(1, 1, sequence)]);
    }
    port.poll().expect("lookahead");
    assert_eq!(
        decoder.borrow().submitted,
        vec![frame(1, 1, 0), frame(1, 1, 1)]
    );
    complete_decode(&decoder, 1);
    assert!(port.poll().expect("hold future result").is_empty());
    assert_eq!(decoder.borrow().submitted.len(), 2);
    complete_decode(&decoder, 0);
    let records = port.poll().expect("apply ordered prefix");
    let revisions = records
        .iter()
        .map(|record| match &record.event {
            DestinationPortEvent::Surface(ClientSurfaceEvent {
                kind: ClientSurfaceEventKind::Commit(commit),
                ..
            }) => commit.revision,
            _ => panic!("unexpected event"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        revisions,
        vec![ClientCommitRevision::new(1), ClientCommitRevision::new(2)]
    );
    assert_eq!(decoder.borrow().submitted[2], frame(1, 1, 2));
    port.state
        .as_mut()
        .expect("state")
        .cancel_surface(surface)
        .expect("cancel last frame");
    complete_decode(&decoder, 2);
    assert!(
        port.poll()
            .expect("discard cancelled completion")
            .is_empty()
    );
}

#[test]
fn lookahead_never_passes_missing_media_or_structural_barriers() {
    for barrier in 0..6 {
        let (mut port, transport, decoder) = destination_port();
        decoder.borrow_mut().capacity = Some(8);
        let surface = surface(ClientSourceId::new(1), 1, 1);
        queue(&transport, surface, 1, &[frame(1, 1, 0)]);
        queue(&transport, surface, 2, &[frame(1, 1, 1)]);
        queue(&transport, surface, 3, &[frame(1, 1, 2)]);
        if barrier == 0 {
            let missing = transport
                .borrow_mut()
                .incoming
                .remove(1)
                .expect("first media");
            port.poll().expect("media missing");
            assert!(decoder.borrow().submitted.is_empty());
            transport.borrow_mut().incoming.push_back(missing);
            port.poll().expect("late earlier media enables lookahead");
            assert_eq!(decoder.borrow().submitted.len(), 2);
        } else {
            {
                let mut incoming = transport.borrow_mut();
                let SourceTransportPacket::Control(SourceEnvelope {
                    message: SourceMessage::Surface(event),
                    ..
                }) = &mut incoming.incoming[2]
                else {
                    panic!("second control");
                };
                let WireClientSurfaceEventKind::Commit(commit) = &mut event.kind else {
                    panic!("commit");
                };
                match barrier {
                    1 => commit.mapped = false,
                    2 => {
                        commit.buffers[0].change = WireSurfaceBufferChange::Retained {
                            metadata: ClientBufferMetadata::new(Extent::new(1, 1), true),
                        }
                    }
                    3 => commit.buffers[0].layer = SurfaceLayerId::new(2),
                    4 => {
                        if let WireSurfaceBufferChange::Replaced { buffer, .. } =
                            &mut commit.buffers[0].change
                        {
                            buffer.frame = frame(1, 2, 0);
                        }
                    }
                    5 => {
                        if let WireSurfaceBufferChange::Replaced { metadata, .. } =
                            &mut commit.buffers[0].change
                        {
                            *metadata = ClientBufferMetadata::new(Extent::new(2, 1), true);
                        }
                    }
                    _ => panic!("case"),
                }
            }
            port.poll().expect("stop at barrier");
            assert_eq!(decoder.borrow().submitted, vec![frame(1, 1, 0)]);
        }
    }
}

#[test]
fn non_commit_successor_is_not_decode_lookahead() {
    let surface = surface(ClientSourceId::new(1), 1, 1);
    let first = tree(surface, 1, &[frame(1, 1, 0)]);
    let next = WireClientSurfaceEvent {
        surface,
        kind: WireClientSurfaceEventKind::Destroyed,
    };
    assert!(!compatible_decode_lookahead(&first, &next));
}

#[test]
fn lookahead_preserves_round_robin_and_cancels_all_prefetched_frames() {
    let (mut port, transport, decoder) = destination_port();
    decoder.borrow_mut().capacity = Some(0);
    for sequence in 0..2 {
        for stream in 1..=3 {
            queue(
                &transport,
                surface(ClientSourceId::new(1), 1, stream),
                sequence + 1,
                &[frame(stream, 1, sequence)],
            );
        }
    }
    port.poll().expect("all backlogged");
    decoder.borrow_mut().capacity = Some(8);
    port.poll().expect("two fair passes");
    assert_eq!(
        decoder.borrow().submitted,
        vec![
            frame(1, 1, 0),
            frame(2, 1, 0),
            frame(3, 1, 0),
            frame(1, 1, 1),
            frame(2, 1, 1),
            frame(3, 1, 1)
        ]
    );
    for stream in 1..=3 {
        port.state
            .as_mut()
            .expect("state")
            .cancel_surface(surface(ClientSourceId::new(1), 1, stream))
            .expect("cancel");
    }
    for index in (0..6).rev() {
        complete_decode(&decoder, index);
    }
    assert!(port.poll().expect("cancelled lookahead").is_empty());
    assert!(port.state.as_ref().expect("state").decoded.is_empty());
    assert!(
        port.state
            .as_ref()
            .expect("state")
            .decode_in_flight
            .is_empty()
    );
}
