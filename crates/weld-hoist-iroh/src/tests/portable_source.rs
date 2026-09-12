//! Public ready/pending registrations must produce the same source traffic.
use super::*;
use std::{cell::Cell, rc::Rc};
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId,
    ClientCommitRevision, ClientEventQueue, ClientSurfaceCommit, ClientSurfaceEvent,
    ClientSurfaceEventKind, ClientSurfaceRole, Extent, SurfaceBufferChange, SurfaceBufferUpdate,
    SurfaceLayerId, ToplevelState, WindowDecoration,
};
use weld_hoist_core::HoistEndpoint;
use weld_hoist_encoded::{
    EncodeBackend, EncodeCompletion, EncodeInput, EncodeRequest, PreparedEncodeInput, SubmitError,
};

struct Encoder {
    submitted: Rc<Cell<usize>>,
    completions: Vec<EncodeCompletion>,
}

impl EncodeBackend for Encoder {
    fn prepare_input(&self, lease: &ClientBufferLease) -> anyhow::Result<PreparedEncodeInput> {
        Ok(PreparedEncodeInput {
            input: EncodeInput::PackedBgra {
                width: 1,
                height: 1,
                pixels: vec![7; 4],
            },
            retained_lease: Some(lease.clone()),
        })
    }
    fn try_submit(&mut self, request: EncodeRequest) -> Result<(), SubmitError<EncodeRequest>> {
        self.submitted.set(self.submitted.get() + 1);
        self.completions.push(EncodeCompletion {
            token: request.token,
            result: Ok(EncodedAccessUnit {
                frame: request.frame,
                timestamp_micros: request.timestamp_micros,
                codec: VideoCodec::Av1,
                kind: EncodedFrameKind::Keyframe,
                payload: vec![7; 4],
            }),
        });
        Ok(())
    }
    fn drain(&mut self) -> Vec<EncodeCompletion> {
        std::mem::take(&mut self.completions)
    }
    fn retire(&mut self, _: MediaStreamId, _: StreamGeneration) -> anyhow::Result<()> {
        Ok(())
    }
}

fn run_registration(pending_mode: bool) -> Vec<Vec<u8>> {
    // Compare ready/manual mapping with pending/automatic mapping as paired entrypoints.
    let directory = rendezvous::tests::ExchangeDirectory::new();
    let ticket = directory.0.join("source.ticket");
    let identity = directory.0.join("destination.identity");
    let source_host = IrohHost::bind(IrohNetwork::Direct).expect("source");
    let destination_host = IrohHost::bind(IrohNetwork::Direct).expect("destination");
    destination_host
        .publish_identity(&identity)
        .expect("identity");
    let mut pending = source_host
        .begin_accept_source(
            &ticket,
            &identity,
            VideoCodec::Av1,
            IrohNotifier::new(|| Ok(())),
            Duration::from_secs(5),
        )
        .expect("pending");
    let connect = || {
        destination_host
            .connect_destination(
                &ticket,
                vec![VideoCodec::Av1],
                IrohNotifier::new(|| Ok(())),
                Duration::from_secs(5),
            )
            .expect("connect")
    };
    let upstream = ClientSourceId::new(0);
    let adapter = ClientSourceId::new(1);
    let surface = ClientSurfaceId::new(ClientId::new(upstream, 1), 1);
    let submissions = Rc::new(Cell::new(0));
    let backend = Box::new(Encoder {
        submitted: submissions.clone(),
        completions: Vec::new(),
    });
    let (registration, endpoint, mut destination) = if pending_mode {
        (
            pending_source_registration_with_backend(
                pending, upstream, adapter, backend, None, None,
            ),
            None,
            None,
        )
    } else {
        let destination = connect();
        let peer = wait_for(|| pending.poll().expect("poll"));
        let (registration, endpoint) =
            source_registration_with_backend(peer, upstream, adapter, adapter, backend, None, None)
                .expect("ready registration");
        (registration, Some(endpoint), Some(destination))
    };
    let mut driver = registration.into_parts().runtime.driver;
    driver.observe_event(&ClientSurfaceEvent {
        surface,
        kind: ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
            parent: None,
            decoration: WindowDecoration::ServerSide,
        })),
    });
    let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
    let buffer = ClientBufferLease::new(
        ClientBufferId::new(upstream, 1),
        ClientBufferUseId::new(upstream, 1),
        metadata,
        Rc::new(()),
        |_| {},
    )
    .expect("lease");
    driver.observe_event(&ClientSurfaceEvent {
        surface,
        kind: ClientSurfaceEventKind::Commit(ClientSurfaceCommit {
            revision: ClientCommitRevision::new(1),
            alpha_mode: Default::default(),
            mapped: true,
            root: None,
            window_geometry: None,
            overlays: Vec::new(),
            inputs: Vec::new(),
            buffers: vec![SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(1),
                change: SurfaceBufferChange::Replaced { metadata, buffer },
            }],
        }),
    });
    assert_eq!(
        submissions.get(),
        0,
        "cached pixels do not encode before mapping"
    );
    if let Some(endpoint) = endpoint {
        driver.apply_command(endpoint.map(HoistSessionId::new(1), surface));
    } else {
        destination = Some(connect());
    }
    let destination = destination.expect("destination");
    let mut packets = Vec::new();
    wait_for(|| {
        driver.drain_events(&mut ClientEventQueue::default());
        packets.extend(destination.drain(ReceiveBudget::ALL).expect("receive"));
        (packets.len() >= 4).then_some(())
    });
    assert_eq!(submissions.get(), 1);
    assert_eq!(packets.len(), 4, "map, role, commit, media");
    destination.disconnect();
    // Control and media use independent streams; compare contents, not arrival order.
    let mut encoded = packets
        .into_iter()
        .map(|packet| match packet {
            SourceTransportPacket::Control(packet) => {
                postcard::to_stdvec(&packet).expect("control")
            }
            SourceTransportPacket::Media(mut packet) => {
                packet.access_unit.timestamp_micros = 0;
                postcard::to_stdvec(&packet).expect("media")
            }
        })
        .collect::<Vec<_>>();
    encoded.sort();
    encoded
}

#[test]
fn ready_and_pending_sources_share_first_frame_semantics() {
    assert_eq!(run_registration(false), run_registration(true));
}
