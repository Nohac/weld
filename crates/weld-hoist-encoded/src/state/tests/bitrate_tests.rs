//! Actuator behavior through real source scheduling with a strict fake encoder.

use super::*;
use crate::EncoderBitrateLimits;

fn controlled_source() -> (
    EncodedSourceState,
    Rc<RefCell<FakeEncoderState>>,
    EncoderRateControl,
) {
    let fake = Rc::new(RefCell::new(FakeEncoderState {
        bitrate_limits: Some(EncoderBitrateLimits::try_new(1, 8000, 8000).expect("limits")),
        generation_limit: Some(2),
        ..Default::default()
    }));
    let source = EncodedSourceState::new(Box::new(FakeEncoder(fake.clone())));
    let control = source.rates.as_ref().expect("rates").control();
    (source, fake, control)
}

fn source_surface() -> ClientSurfaceId {
    surface(ClientSourceId::new(1), 2, 3)
}

fn enqueue(
    source: &mut EncodedSourceState,
    revision: u64,
    layers: &[u64],
    width: u32,
) -> Result<()> {
    let metadata = ClientBufferMetadata::new(Extent::new(width, 1), true);
    let buffers = layers
        .iter()
        .map(|layer| SurfaceBufferUpdate {
            layer: SurfaceLayerId::new(*layer),
            change: SurfaceBufferChange::Replaced {
                metadata,
                buffer: shm_lease(ClientSourceId::new(1), revision * 10 + layer, 10, metadata),
            },
        })
        .collect();
    source.enqueue(
        HoistSessionId::new(1),
        commit(source_surface(), revision, buffers),
    )
}

fn finish(source: &mut EncodedSourceState, fake: &Rc<RefCell<FakeEncoderState>>, index: usize) {
    let (token, frame, _) = fake.borrow().submitted[index].clone();
    complete(fake, token, frame, 1);
    source.drain().expect("completion");
}

fn deliver_output(source: &mut EncodedSourceState) {
    source.take_output();
    source.schedule().expect("local output capacity recovered");
}

#[test]
fn encoder_requests_use_bootstrap_or_negotiated_cadence_with_real_backend_limits() {
    for (ceiling, requested, expected) in [
        (None, None, 60_000),
        (None, Some(30_000), 30_000),
        (None, Some(90_000), 90_000),
        (None, Some(120_000), 120_000),
        (None, Some(59_940), 59_940),
        (Some(PresentationRate::HZ_60), Some(90_000), 60_000),
    ] {
        let fake = Rc::new(RefCell::new(FakeEncoderState {
            default_frame_rate: Some(PresentationRate::HZ_60),
            frame_rate_limit: ceiling,
            ..Default::default()
        }));
        let mut source = EncodedSourceState::new(Box::new(FakeEncoder(fake.clone())));
        if let Some(requested) = requested {
            source.pacing.set(
                source_surface(),
                ClientPresentationClaim::Active {
                    rate: Some(PresentationRate::try_from(requested).expect("rate")),
                },
            );
        }
        enqueue(&mut source, 1, &[1], 1).expect("initial batch");
        assert_eq!(
            fake.borrow().submitted_frame_rates,
            vec![Some(PresentationRate::try_from(expected).expect("rate"))]
        );
        assert!(
            fake.borrow().retirements.is_empty(),
            "initial negotiation does not rebuild"
        );
    }
}

#[test]
fn cadence_changes_keep_prepared_layers_frozen_and_combine_with_resize_and_bitrate() {
    let (mut source, fake, control) = controlled_source();
    let initial = PresentationRate::try_from(90_000).expect("rate");
    let latest = PresentationRate::try_from(59_940).expect("rate");
    source.pacing.set(
        source_surface(),
        ClientPresentationClaim::Active {
            rate: Some(initial),
        },
    );
    enqueue(&mut source, 1, &[1, 2], 1).expect("first batch");
    for stream in control.streams().expect("streams") {
        control.request(stream.stream, 4000).expect("bitrate");
    }
    for rate in [PresentationRate::try_from(120_000).expect("rate"), latest] {
        source.pacing.set(
            source_surface(),
            ClientPresentationClaim::Active { rate: Some(rate) },
        );
    }
    enqueue(&mut source, 2, &[1, 2], 2).expect("next batch with resized layers");
    finish(&mut source, &fake, 0);
    assert_eq!(
        fake.borrow().submitted_frame_rates,
        vec![Some(initial), Some(initial)]
    );
    assert!(fake.borrow().retirements.is_empty());
    finish(&mut source, &fake, 1);
    source.pacing.reset(source_surface());
    deliver_output(&mut source);
    finish(&mut source, &fake, 2);
    finish(&mut source, &fake, 3);
    let encoder = fake.borrow();
    assert_eq!(
        encoder.submitted_frame_rates,
        vec![Some(initial), Some(initial), Some(latest), Some(latest)]
    );
    assert_eq!(
        encoder.submitted_bitrates,
        vec![Some(8000), Some(8000), Some(4000), Some(4000)]
    );
    assert_eq!(
        encoder
            .submitted
            .iter()
            .map(|(_, frame, _)| frame.generation.raw())
            .collect::<Vec<_>>(),
        vec![1, 1, 2, 2]
    );
    assert_eq!(
        encoder.retirements.len(),
        2,
        "one replacement per stream for all changes"
    );
}

#[test]
fn negotiated_cadence_survives_paused_hidden_buffers_without_generation_churn() {
    let fake = Rc::new(RefCell::new(FakeEncoderState {
        default_frame_rate: Some(PresentationRate::HZ_60),
        ..Default::default()
    }));
    let mut source = EncodedSourceState::new(Box::new(FakeEncoder(fake.clone())));
    let rate = PresentationRate::try_from(90_000).expect("rate");
    let active = ClientPresentationClaim::Active { rate: Some(rate) };
    source.pacing.set(source_surface(), active);
    enqueue(&mut source, 1, &[1], 1).expect("initial");
    finish(&mut source, &fake, 0);
    deliver_output(&mut source);
    for (index, claim) in [
        ClientPresentationClaim::Paused,
        ClientPresentationClaim::Release,
        active,
        active,
    ]
    .into_iter()
    .enumerate()
    {
        source.pacing.set(source_surface(), claim);
        let revision = index as u64 + 2;
        let lease = shm_lease(
            ClientSourceId::new(1),
            revision,
            10,
            ClientBufferMetadata::new(Extent::new(1, 1), true),
        );
        let mut event = one_buffer_commit(source_surface(), revision, 1, lease);
        if let ClientSurfaceEventKind::Commit(commit) = &mut event.kind {
            commit.mapped = false;
        }
        source
            .enqueue(HoistSessionId::new(1), event)
            .expect("hidden replacement");
        finish(&mut source, &fake, index + 1);
        deliver_output(&mut source);
    }
    let encoder = fake.borrow();
    assert_eq!(encoder.submitted_frame_rates, vec![Some(rate); 5]);
    assert!(encoder.retirements.is_empty());
}

#[test]
fn multilayer_batch_stays_frozen_and_next_admission_admits_latest_rates() {
    let (mut source, fake, control) = controlled_source();
    enqueue(&mut source, 1, &[1, 2], 1).expect("first batch");
    let streams = control.streams().expect("registered streams");
    let first = streams[0].stream;
    let second = streams[1].stream;
    control.request(first, 6000).expect("intermediate");
    let wanted_first = control.request(first, 4000).expect("latest first");
    let wanted_second = control.request(second, 3000).expect("latest second");
    enqueue(&mut source, 2, &[1, 2], 1).expect("next batch waits");
    assert_eq!(fake.borrow().submitted.len(), 1);
    finish(&mut source, &fake, 0);
    assert_eq!(
        fake.borrow().submitted_bitrates,
        vec![Some(8000), Some(8000)]
    );
    assert!(
        fake.borrow().retirements.is_empty(),
        "second old-rate layer is still active"
    );
    finish(&mut source, &fake, 1);
    assert_eq!(
        fake.borrow().submitted.len(),
        2,
        "unsent completed output blocks next batch"
    );
    for status in control.streams().expect("old rates") {
        assert_eq!(
            status.applied.expect("old packet").request.bits_per_second,
            8000
        );
    }
    deliver_output(&mut source);
    assert_eq!(fake.borrow().submitted_bitrates[2], Some(4000));
    assert_eq!(fake.borrow().retirements.len(), 2);
    finish(&mut source, &fake, 2);
    assert_eq!(fake.borrow().submitted_bitrates[3], Some(3000));
    finish(&mut source, &fake, 3);
    let statuses = control.streams().expect("new packets");
    assert_eq!(statuses[0].applied.expect("first").request, wanted_first);
    assert_eq!(statuses[1].applied.expect("second").request, wanted_second);
    assert_eq!(
        fake.borrow()
            .submitted
            .iter()
            .map(|(_, frame, _)| frame.generation.raw())
            .collect::<Vec<_>>(),
        vec![1, 1, 2, 2]
    );
    assert_eq!(
        fake.borrow().generations.len(),
        2,
        "no overlapping encoder generations"
    );
    deliver_output(&mut source);
    enqueue(&mut source, 3, &[1], 1).expect("remove second layer");
    assert!(control.request(second, 2000).is_err());
    assert_eq!(control.streams().expect("only live streams").len(), 1);
}

#[test]
fn resize_and_rate_rotate_once_and_identical_requests_do_not_churn() {
    let (mut source, fake, control) = controlled_source();
    enqueue(&mut source, 1, &[1], 1).expect("initial");
    finish(&mut source, &fake, 0);
    deliver_output(&mut source);
    let stream = control.streams().expect("stream")[0].stream;
    let request = control.request(stream, 4000).expect("lower");
    assert_eq!(fake.borrow().submitted.len(), 1, "no fabricated pixels");
    enqueue(&mut source, 2, &[1], 2).expect("resize plus rate");
    assert_eq!(fake.borrow().submitted[1].1.generation.raw(), 2);
    assert_eq!(fake.borrow().retirements.len(), 1);
    finish(&mut source, &fake, 1);
    deliver_output(&mut source);
    assert_eq!(control.request(stream, 4000).expect("same rate"), request);
    enqueue(&mut source, 3, &[1], 2).expect("same configuration");
    assert_eq!(fake.borrow().submitted[2].1.generation.raw(), 2);
    assert_eq!(fake.borrow().submitted[2].1.sequence, 1);
    assert_eq!(fake.borrow().retirements.len(), 1);
}

#[test]
fn cancellation_retires_control_and_disconnect_expires_all_handles() {
    let (mut source, fake, control) = controlled_source();
    enqueue(&mut source, 1, &[1, 2], 1).expect("initial");
    let stream = control.streams().expect("stream")[0].stream;
    control.request(stream, 4000).expect("pending");
    source.cancel_surface(source_surface()).expect("withdraw");
    assert!(control.streams().expect("retired").is_empty());
    assert!(control.request(stream, 2000).is_err());
    finish(&mut source, &fake, 0);
    assert!(control.streams().expect("late completion").is_empty());
    let transport = FakeSourceTransport(Rc::new(RefCell::new(FakeSourceTransportState::default())));
    let mut port = EncodedSourcePort {
        output: SourceOutput::default(),
        transport,
        state: Some(source),
    };
    assert!(port.encoder_rate_control().is_some());
    port.disconnect();
    assert!(control.streams().is_err());
    assert!(port.encoder_rate_control().is_none());
}

#[test]
fn failed_or_mismatched_replacement_never_becomes_applied() {
    for failure in 0..4 {
        let (mut source, fake, control) = controlled_source();
        enqueue(&mut source, 1, &[1], 1).expect("initial");
        finish(&mut source, &fake, 0);
        deliver_output(&mut source);
        let previous = control.streams().expect("initial status")[0];
        let requested = control.request(previous.stream, 4000).expect("lower");
        enqueue(&mut source, 2, &[1], 1).expect("replacement");
        let (token, frame, _) = fake.borrow().submitted[1].clone();
        let result = if failure == 0 {
            Err(anyhow::anyhow!("codec failure"))
        } else {
            Ok(EncodedAccessUnit {
                frame: if failure == 1 {
                    MediaFrameId {
                        sequence: 99,
                        ..frame
                    }
                } else {
                    frame
                },
                codec: VideoCodec::H264,
                timestamp_micros: 1,
                payload: vec![1],
                kind: if failure == 2 {
                    EncodedFrameKind::Delta
                } else {
                    EncodedFrameKind::Keyframe
                },
            })
        };
        fake.borrow_mut().completions.push(EncodeCompletion {
            token: if failure == 3 { token + 1 } else { token },
            result,
        });
        assert!(source.drain().is_err());
        let status = control.streams().expect("failed status")[0];
        assert_eq!(status.requested, requested);
        assert_eq!(status.applied, previous.applied);
    }
}

#[test]
fn rejected_submission_clears_pending_confirmation_without_claiming_application() {
    let (mut source, fake, control) = controlled_source();
    enqueue(&mut source, 1, &[1], 1).expect("initial");
    finish(&mut source, &fake, 0);
    deliver_output(&mut source);
    let previous = control.streams().expect("initial status")[0];
    control.request(previous.stream, 4000).expect("lower");
    fake.borrow_mut().generation_limit = Some(0);
    assert!(enqueue(&mut source, 2, &[1], 1).is_err());
    let status = control.streams().expect("rejected status")[0];
    assert!(status.submitted.is_none());
    assert_eq!(status.applied, previous.applied);
}

#[test]
fn absent_registry_entry_keeps_frozen_rate_instead_of_backend_default() {
    let (mut source, fake, control) = controlled_source();
    enqueue(&mut source, 1, &[1], 1).expect("initial");
    finish(&mut source, &fake, 0);
    deliver_output(&mut source);
    let stream = control.streams().expect("stream")[0].stream;
    control.request(stream, 4000).expect("lower");
    enqueue(&mut source, 2, &[1], 1).expect("rate replacement");
    finish(&mut source, &fake, 1);
    deliver_output(&mut source);
    source.rates.as_ref().expect("rates").remove(stream);
    enqueue(&mut source, 3, &[1], 1).expect("keep frozen configuration");
    assert_eq!(fake.borrow().submitted_bitrates[2], Some(4000));
    assert_eq!(fake.borrow().submitted[2].1.generation.raw(), 2);
}
