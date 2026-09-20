//! Reuse the live direct-peer fixture to exercise public receiver registration.

use super::*;
use anyhow::Result;
use std::rc::Rc;
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId,
    ClientCommitRevision, ClientEventQueue, ClientSurfaceEventKind, Extent, SurfaceAlphaMode,
    SurfaceBufferChange, SurfaceLayerId, WireClientSurfaceCommit, WireClientSurfaceEvent,
    WireClientSurfaceEventKind, WireSurfaceBufferChange, WireSurfaceBufferUpdate,
};
use weld_hoist_encoded::{
    DecodeBackend, DecodeCompletion, DecodeRequest, DecodedFrame, DecodedFramePublisher,
    SubmitError,
};
use weld_hoist_protocol::EncodedBuffer;

#[derive(Default)]
struct Decoder(Vec<DecodeCompletion<Vec<u8>>>);

impl DecodeBackend for Decoder {
    type Output = Vec<u8>;
    fn try_submit(&mut self, request: DecodeRequest) -> Result<(), SubmitError<DecodeRequest>> {
        self.0.push(DecodeCompletion {
            token: request.token,
            timing: None,
            result: Ok(vec![DecodedFrame {
                frame: request.access_unit.frame,
                buffer: request.access_unit.payload,
            }]),
        });
        Ok(())
    }
    fn drain(&mut self) -> (Vec<DecodeCompletion<Vec<u8>>>, Option<anyhow::Error>) {
        (std::mem::take(&mut self.0), None)
    }
    fn retire(&mut self, _: MediaStreamId, _: StreamGeneration) -> Result<()> {
        Ok(())
    }
}

struct Publisher;
struct TestClientImporter;

impl DecodedFramePublisher for Publisher {
    type Buffer = Vec<u8>;
    type ClientImporter = TestClientImporter;
    fn client_importer(&self) -> TestClientImporter {
        TestClientImporter
    }
    fn publish(
        &mut self,
        bytes: Vec<u8>,
        buffer: ClientBufferId,
        use_id: ClientBufferUseId,
    ) -> Result<ClientBufferLease> {
        Ok(ClientBufferLease::new(
            buffer,
            use_id,
            ClientBufferMetadata::new(Extent::new(1, 1), true),
            Rc::new(bytes),
            |_| {},
        )?)
    }
}

pub(super) fn check_registration(source: &IrohSourcePeer, destination: IrohDestinationPeer) {
    let upstream = ClientSourceId::new(0);
    let target = ClientSourceId::new(9);
    let registration = destination_registration_with_backend(
        destination,
        upstream,
        target,
        Publisher,
        Box::new(Decoder::default()),
        None,
    );
    let mut parts = registration.into_parts();
    assert!(parts.importer.importer.is::<TestClientImporter>());
    let session = HoistSessionId::new(2);
    let surface = ClientSurfaceId::new(ClientId::new(upstream, 8), 9);
    let frame = MediaFrameId::new(MediaStreamId::new(77), StreamGeneration::new(1), 0);
    send_source(
        source,
        SourceTransportPacket::Control(SourceEnvelope {
            session,
            message: SourceMessage::Mapped { surface },
        }),
    )
    .expect("map");
    send_source(
        source,
        SourceTransportPacket::Control(SourceEnvelope {
            session,
            message: SourceMessage::Surface(WireClientSurfaceEvent {
                surface,
                kind: WireClientSurfaceEventKind::Commit(WireClientSurfaceCommit {
                    revision: ClientCommitRevision::new(1),
                    alpha_mode: SurfaceAlphaMode::Discarded,
                    mapped: true,
                    root: None,
                    window_geometry: None,
                    overlays: Vec::new(),
                    inputs: Vec::new(),
                    buffers: vec![WireSurfaceBufferUpdate {
                        layer: SurfaceLayerId::new(1),
                        change: WireSurfaceBufferChange::Replaced {
                            metadata: ClientBufferMetadata::new(Extent::new(1, 1), true),
                            buffer: EncodedBuffer { frame },
                        },
                    }],
                }),
            }),
        }),
    )
    .expect("commit");
    send_source(
        source,
        SourceTransportPacket::Media(MediaEnvelope {
            session,
            access_unit: EncodedAccessUnit {
                frame,
                codec: VideoCodec::H264,
                kind: EncodedFrameKind::Keyframe,
                timestamp_micros: 1,
                payload: vec![10, 20, 30],
            },
        }),
    )
    .expect("test media, not a hardware bitstream");
    let mut events = ClientEventQueue::default();
    let event = wait_for(|| {
        parts.runtime.driver.drain_events(&mut events);
        events.pop_front()
    });
    assert_eq!(event.surface.source(), target);
    let ClientSurfaceEventKind::Commit(commit) = event.kind else {
        panic!("commit");
    };
    let SurfaceBufferChange::Replaced { buffer, .. } = &commit.buffers[0].change else {
        panic!("buffer");
    };
    assert_eq!(buffer.buffer().source(), target);
    assert_eq!(
        buffer.access::<Vec<u8>>().expect("portable payload"),
        &[10, 20, 30]
    );
}
