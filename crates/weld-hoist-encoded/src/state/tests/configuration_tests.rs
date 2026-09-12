//! Shared construction and consumer-lifetime contracts, without GPU work.
use super::*;
use crate::EncoderBitrateLimits;

#[test]
fn configured_source_applies_shared_budget_before_first_frame() {
    let transport = Rc::new(RefCell::new(FakeSourceTransportState::default()));
    let encoder = Rc::new(RefCell::new(FakeEncoderState {
        bitrate_limits: Some(
            EncoderBitrateLimits::try_new(128_000, 8_000_000, 8_000_000).expect("limits"),
        ),
        ..Default::default()
    }));
    let mut port = EncodedSourcePort::configured(
        FakeSourceTransport(transport.clone()),
        Box::new(FakeEncoder(encoder.clone())),
        EncodedSourceOptions {
            bitrate_budget: Some(SharedBitrateBudget::new(4_000_000).expect("budget")),
            access_unit_dump: None,
        },
    )
    .expect("configured port");
    assert!(port.encoder_rate_control().is_some());
    let source = ClientSourceId::new(1);
    port.submit(SourcePortCommand::Surface {
        session: HoistSessionId::new(1),
        event: one_buffer_commit(
            surface(source, 1, 1),
            1,
            1,
            shm_lease(
                source,
                1,
                7,
                ClientBufferMetadata::new(Extent::new(1, 1), true),
            ),
        ),
    })
    .expect("first frame");
    assert!(
        matches!(encoder.borrow().submitted_bitrates.as_slice(), [Some(rate)]
        if (128_000..=4_000_000).contains(rate)),
        "first frame must use the shared allocation, not the 8 Mbps backend default"
    );
    assert!(!transport.borrow().disconnected);
}

#[test]
fn configured_source_closes_transport_when_budget_has_no_actuator() {
    let transport = Rc::new(RefCell::new(FakeSourceTransportState::default()));
    let result = EncodedSourcePort::configured(
        FakeSourceTransport(transport.clone()),
        Box::new(FakeEncoder(Rc::new(RefCell::new(
            FakeEncoderState::default(),
        )))),
        EncodedSourceOptions {
            bitrate_budget: Some(SharedBitrateBudget::new(4_000_000).expect("budget")),
            access_unit_dump: None,
        },
    );
    assert!(
        result
            .err()
            .expect("no actuator")
            .to_string()
            .contains("bitrate control")
    );
    assert!(transport.borrow().disconnected);
}

#[test]
fn configured_source_closes_transport_when_dump_setup_fails() {
    let transport = Rc::new(RefCell::new(FakeSourceTransportState::default()));
    let result = EncodedSourcePort::configured(
        FakeSourceTransport(transport.clone()),
        Box::new(FakeEncoder(Rc::new(RefCell::new(
            FakeEncoderState::default(),
        )))),
        EncodedSourceOptions {
            bitrate_budget: None,
            access_unit_dump: Some((
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
                VideoCodec::Av1,
            )),
        },
    );
    assert!(result.is_err(), "a regular file is not a dump directory");
    assert!(transport.borrow().disconnected);
}

#[test]
fn source_progress_does_not_depend_on_an_optional_presentation_consumer() {
    for presented in [false, true] {
        let encoder = Rc::new(RefCell::new(FakeEncoderState {
            retain_input: true,
            ..Default::default()
        }));
        let transport = Rc::new(RefCell::new(FakeSourceTransportState::default()));
        let mut port = EncodedSourcePort::configured(
            FakeSourceTransport(transport.clone()),
            Box::new(FakeEncoder(encoder.clone())),
            EncodedSourceOptions::default(),
        )
        .expect("source");
        let released = Rc::new(Cell::new(0));
        let mut presentation_leases = Vec::new();
        let source = ClientSourceId::new(1);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        for revision in 1..=3 {
            let release = released.clone();
            let lease = ClientBufferLease::new(
                ClientBufferId::new(source, revision),
                ClientBufferUseId::new(source, revision),
                metadata,
                Rc::new(vec![7_u8; 4]),
                move |_| release.set(release.get() + 1),
            )
            .expect("lease");
            if presented {
                presentation_leases.push(lease.clone());
            }
            port.submit(SourcePortCommand::Surface {
                session: HoistSessionId::new(1),
                event: one_buffer_commit(surface(source, 1, 1), revision, 1, lease),
            })
            .expect("frame");
            let (token, frame, _) = encoder
                .borrow()
                .submitted
                .last()
                .expect("submitted")
                .clone();
            assert_eq!(released.get(), if presented { 0 } else { revision - 1 });
            complete(&encoder, token, frame, 7);
            port.poll().expect("completion");
            port.progress_after_destination()
                .expect("no reverse messages needed");
            assert_eq!(encoder.borrow().submitted.len(), revision as usize);
            assert_eq!(released.get(), if presented { 0 } else { revision });
        }
        let frames = transport
            .borrow()
            .sent
            .iter()
            .filter(|packet| matches!(packet, SourceTransportPacket::Media(_)))
            .count();
        assert_eq!(
            frames, 3,
            "even a presenter retaining every lease must not stall encoding"
        );
        drop(presentation_leases);
        assert_eq!(released.get(), 3);
    }
}
