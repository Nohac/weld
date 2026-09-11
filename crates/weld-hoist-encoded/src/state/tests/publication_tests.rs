//! Real receiver publication with non-native owned buffers and release tracking.

use super::decode_tests::tree;
use super::*;
use weld_hoist_core::DestinationRelayAdapter;

#[derive(Default)]
struct Releases {
    attempted: Vec<u64>,
    buffers: Vec<u64>,
    leases: Vec<u64>,
}

struct TrackedBuffer {
    value: u64,
    releases: Rc<RefCell<Releases>>,
}

impl Drop for TrackedBuffer {
    fn drop(&mut self) {
        self.releases.borrow_mut().buffers.push(self.value);
    }
}

struct Decoder {
    releases: Rc<RefCell<Releases>>,
    ready: Vec<DecodeCompletion<TrackedBuffer>>,
}

impl DecodeBackend for Decoder {
    type Output = TrackedBuffer;

    fn try_submit(&mut self, request: DecodeRequest) -> Result<(), SubmitError<DecodeRequest>> {
        self.ready.push(DecodeCompletion {
            token: request.token,
            timing: None,
            result: Ok(vec![DecodedFrame {
                frame: request.access_unit.frame,
                buffer: TrackedBuffer {
                    value: request.access_unit.frame.stream.raw(),
                    releases: self.releases.clone(),
                },
            }]),
        });
        Ok(())
    }

    fn drain(&mut self) -> (Vec<DecodeCompletion<TrackedBuffer>>, Option<anyhow::Error>) {
        (std::mem::take(&mut self.ready), None)
    }

    fn retire(&mut self, _: MediaStreamId, _: StreamGeneration) -> Result<()> {
        Ok(())
    }
}

struct Publisher {
    releases: Rc<RefCell<Releases>>,
    fail_on: Option<u64>,
}

impl DecodedFramePublisher for Publisher {
    type Buffer = TrackedBuffer;
    type ClientImporter = TestClientImporter;

    fn client_importer(&self) -> TestClientImporter {
        TestClientImporter
    }

    fn publish(
        &mut self,
        output: TrackedBuffer,
        buffer: ClientBufferId,
        use_id: ClientBufferUseId,
    ) -> Result<ClientBufferLease> {
        let value = output.value;
        self.releases.borrow_mut().attempted.push(value);
        ensure!(self.fail_on != Some(value), "test publication failed");
        let releases = self.releases.clone();
        Ok(ClientBufferLease::new(
            buffer,
            use_id,
            ClientBufferMetadata::new(Extent::new(1, 1), true),
            Rc::new(output),
            move |_| releases.borrow_mut().leases.push(value),
        )?)
    }
}

type Port = EncodedDestinationPort<FakeDestinationTransport, Publisher>;

struct Fixture {
    port: Port,
    transport: Rc<RefCell<FakeDestinationTransportState>>,
    releases: Rc<RefCell<Releases>>,
}

impl Fixture {
    fn new(fail_on: Option<u64>) -> Self {
        let releases = Rc::new(RefCell::new(Releases::default()));
        let transport = Rc::new(RefCell::new(FakeDestinationTransportState::default()));
        let port = EncodedDestinationPort::new(
            FakeDestinationTransport(transport.clone()),
            Box::new(Decoder {
                releases: releases.clone(),
                ready: Vec::new(),
            }),
            ClientSourceDescriptor::new(
                ClientSourceId::new(9),
                weld_client::ClientProvenance::Relocated,
            ),
            Publisher {
                releases: releases.clone(),
                fail_on,
            },
        );
        Self {
            port,
            transport,
            releases,
        }
    }

    fn queue(&self, window: u64, streams: &[u64]) {
        let session = HoistSessionId::new(1);
        let surface = surface(ClientSourceId::new(1), 1, window);
        let frames = streams
            .iter()
            .map(|stream| {
                MediaFrameId::new(MediaStreamId::new(*stream), StreamGeneration::new(1), 0)
            })
            .collect::<Vec<_>>();
        let mut transport = self.transport.borrow_mut();
        transport
            .incoming
            .push_back(SourceTransportPacket::Control(SourceEnvelope {
                session,
                message: SourceMessage::Surface(tree(surface, 1, &frames)),
            }));
        transport.incoming.extend(
            frames
                .into_iter()
                .map(|frame| SourceTransportPacket::Media(encoded_media(session, frame))),
        );
    }
}

fn sorted(values: &[u64]) -> Vec<u64> {
    let mut values = values.to_vec();
    values.sort_unstable();
    values
}

#[test]
fn published_lease_outlives_the_port_and_releases_once_after_its_final_clone() {
    let mut fixture = Fixture::new(None);
    fixture.queue(1, &[1]);
    assert!(fixture.port.poll().expect("admit decode").is_empty());
    assert!(fixture.releases.borrow().attempted.is_empty());
    let records = fixture.port.poll().expect("publish");
    let DestinationPortEvent::Surface(ClientSurfaceEvent {
        kind: ClientSurfaceEventKind::Commit(commit),
        ..
    }) = &records[0].event
    else {
        panic!("commit");
    };
    let SurfaceBufferChange::Replaced { buffer, .. } = &commit.buffers[0].change else {
        panic!("buffer");
    };
    assert_eq!(buffer.buffer().source(), ClientSourceId::new(9));
    let lease = buffer.clone();
    drop(records);
    drop(fixture.port);
    assert!(fixture.releases.borrow().buffers.is_empty());
    assert!(fixture.releases.borrow().leases.is_empty());
    assert_eq!(lease.access::<TrackedBuffer>().expect("payload").value, 1);
    drop(lease);
    assert_eq!(fixture.releases.borrow().buffers, [1]);
    assert_eq!(fixture.releases.borrow().leases, [1]);
}

#[test]
fn cancellation_drops_completed_output_without_publishing_it() {
    let mut fixture = Fixture::new(None);
    fixture.queue(1, &[1]);
    fixture.port.poll().expect("submit");
    fixture
        .port
        .state
        .as_mut()
        .expect("state")
        .cancel_surface(surface(ClientSourceId::new(1), 1, 1))
        .expect("cancel");
    assert!(fixture.port.poll().expect("discard completion").is_empty());
    assert!(fixture.releases.borrow().attempted.is_empty());
    assert_eq!(fixture.releases.borrow().buffers, [1]);
    assert!(fixture.releases.borrow().leases.is_empty());
}

#[test]
fn failed_publication_discards_earlier_commit_and_partial_layers_then_disconnect_releases_rest() {
    let mut fixture = Fixture::new(Some(3));
    fixture.queue(1, &[1]);
    fixture.queue(2, &[2, 3, 4]);
    fixture.queue(3, &[5]);
    assert!(fixture.port.poll().expect("admit all").is_empty());
    // Admission rotates ready surfaces. Fix publication order here to isolate
    // the earlier-success/partial-failure case from scheduler policy.
    fixture.port.state.as_mut().expect("state").ready_surfaces = (1..=3)
        .map(|window| surface(ClientSourceId::new(1), 1, window))
        .collect();
    assert!(
        fixture.port.poll().is_err(),
        "no commit from this poll escapes"
    );
    assert_eq!(fixture.releases.borrow().attempted, [1, 2, 3]);
    assert_eq!(sorted(&fixture.releases.borrow().leases), [1, 2]);
    assert_eq!(sorted(&fixture.releases.borrow().buffers), [1, 2, 3]);
    // The relay performs this teardown on the propagated error.
    fixture.port.disconnect();
    assert!(fixture.transport.borrow().disconnected);
    assert_eq!(sorted(&fixture.releases.borrow().buffers), [1, 2, 3, 4, 5]);
    assert_eq!(sorted(&fixture.releases.borrow().leases), [1, 2]);
}

#[test]
fn relay_disconnects_on_publication_failure_without_delivering_a_partial_commit() {
    let mut fixture = Fixture::new(Some(3));
    fixture.queue(1, &[1]);
    fixture.queue(2, &[2, 3, 4]);
    assert!(fixture.port.poll().expect("admit all").is_empty());
    fixture.port.state.as_mut().expect("state").ready_surfaces = (1..=2)
        .map(|window| surface(ClientSourceId::new(1), 1, window))
        .collect();
    let descriptor = ClientSourceDescriptor::new(
        ClientSourceId::new(9),
        weld_client::ClientProvenance::Relocated,
    );
    let mut relay = DestinationRelayAdapter::new(ClientSourceId::new(1), descriptor, fixture.port);
    let mut events = ClientEventQueue::default();
    relay.drain_events(&mut events);
    relay.drain_events(&mut events);
    assert!(fixture.transport.borrow().disconnected);
    assert!(events.is_empty());
    assert_eq!(sorted(&fixture.releases.borrow().buffers), [1, 2, 3, 4]);
    assert_eq!(sorted(&fixture.releases.borrow().leases), [1, 2]);
}
