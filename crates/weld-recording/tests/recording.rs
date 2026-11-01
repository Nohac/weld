use anyhow::Result;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId,
    ClientCommitRevision, ClientId, ClientSourceId, ClientSurfaceCommit, ClientSurfaceId,
    ClientSurfaceState, ComposedBuffer, Extent, LogicalSize, SurfaceBufferChange,
    SurfaceBufferUpdate, SurfaceLayerId,
};
use weld_hoist_encoded::{
    EncodeBackend, EncodeCompletion, EncodeInput, EncodeRequest, PreparedEncodeInput, SubmitError,
};
use weld_media::{
    EncodedAccessUnit, EncodedFrameKind, MediaStreamId, StreamGeneration, VideoCodec,
};
use weld_recording::{Codec, Options, Recorder, Region, Status};

struct Encoder {
    completed: Vec<EncodeCompletion>,
    sizes: Rc<RefCell<Vec<Extent>>>,
}
impl EncodeBackend for Encoder {
    fn configure_stream_mode(&mut self, mode: weld_client::SurfaceStreamMode) -> Result<()> {
        assert_eq!(mode, weld_client::SurfaceStreamMode::Composited);
        Ok(())
    }
    fn prepare_input(&self, lease: &ClientBufferLease) -> Result<PreparedEncodeInput> {
        let scene = lease.access::<ComposedBuffer>().expect("composition");
        self.sizes.borrow_mut().push(scene.extent);
        Ok(PreparedEncodeInput {
            input: EncodeInput::PackedBgra {
                width: scene.extent.width,
                height: scene.extent.height,
                pixels: vec![],
            },
            retained_lease: Some(lease.clone()),
        })
    }
    fn try_submit(&mut self, request: EncodeRequest) -> Result<(), SubmitError<EncodeRequest>> {
        self.completed.push(EncodeCompletion {
            token: request.token,
            result: Ok(EncodedAccessUnit {
                frame: request.frame,
                codec: VideoCodec::Av1,
                kind: if request.frame.sequence == 0 {
                    EncodedFrameKind::Keyframe
                } else {
                    EncodedFrameKind::Delta
                },
                timestamp_micros: request.timestamp_micros,
                payload: vec![1],
            }),
        });
        Ok(())
    }
    fn drain(&mut self) -> Vec<EncodeCompletion> {
        std::mem::take(&mut self.completed)
    }
    fn retire(&mut self, _: MediaStreamId, _: StreamGeneration) -> Result<()> {
        Ok(())
    }
}

fn commit(revision: u64) -> ClientSurfaceCommit {
    let source = ClientSourceId::new(0);
    let scene = ComposedBuffer {
        extent: Extent::new(200, 100),
        logical_size: LogicalSize {
            width: 100.,
            height: 50.,
        },
        layers: vec![],
    };
    let buffer = ClientBufferLease::new(
        ClientBufferId::new(source, 1),
        ClientBufferUseId::new(source, revision),
        ClientBufferMetadata::new(scene.extent, true),
        Rc::new(scene),
        |_| {},
    )
    .expect("lease");
    ClientSurfaceState {
        revision: ClientCommitRevision::new(revision),
        alpha_mode: Default::default(),
        mapped: true,
        root: None,
        window_geometry: None,
        overlays: vec![],
        inputs: vec![],
        buffers: vec![SurfaceBufferUpdate {
            layer: SurfaceLayerId::new(1),
            change: SurfaceBufferChange::Replaced {
                metadata: buffer.metadata(),
                buffer,
            },
        }],
    }
    .into()
}

#[test]
fn recording_reuses_stream_coalescing_and_keeps_original_observation_times() {
    let now = Instant::now();
    let sizes = Rc::new(RefCell::new(Vec::new()));
    let encoder = Encoder {
        completed: vec![],
        sizes: sizes.clone(),
    };
    let surface = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 1), 1);
    let mut recorder = Recorder::new(
        "test".into(),
        surface,
        commit(10),
        Options {
            duration_ms: 100,
            region: Some(Region {
                x: 10.,
                y: 5.,
                width: 30.,
                height: 20.,
            }),
            codec: Codec::Av1,
            after_sequence: None,
        },
        Box::new(encoder),
        now,
    )
    .expect("recorder");
    recorder
        .observe(&commit(11), now + Duration::from_millis(10))
        .expect("observe");
    recorder
        .observe(&commit(12), now + Duration::from_millis(20))
        .expect("observe");
    recorder
        .observe(&commit(13), now + Duration::from_millis(30))
        .expect("observe");
    for _ in 0..5 {
        recorder
            .tick(now + Duration::from_millis(100))
            .expect("drain");
    }
    assert!(
        recorder
            .tick(now + Duration::from_millis(100))
            .expect("complete")
    );
    let clip = recorder.finish();
    assert!(matches!(clip.info.status, Status::Complete));
    assert_eq!(clip.info.submitted_frames, 4);
    assert_eq!(
        clip.info.frame_count + clip.info.dropped_frames.expect("final dropped count"),
        4
    );
    assert!(
        clip.info.dropped_frames.expect("final dropped count") > 0,
        "queued frames use the streaming coalescer"
    );
    assert_eq!(clip.info.frames.first().expect("initial").revision, "10");
    assert_eq!(clip.info.frames.last().expect("final").revision, "13");
    assert_eq!(clip.info.frames.last().expect("time").time_us, 30_000);
    assert!(
        sizes
            .borrow()
            .iter()
            .all(|size| *size == Extent::new(60, 40))
    );
}

#[test]
fn recording_rejects_invalid_limits_and_out_of_bounds_crop() {
    let options = Options {
        duration_ms: 0,
        codec: Codec::Av1,
        region: None,
        after_sequence: None,
    };
    assert!(options.validate().is_err());
    assert!(
        Region {
            x: f32::NAN,
            y: 0.,
            width: 1.,
            height: 1.
        }
        .validate(100., 100.)
        .is_err()
    );
    assert!(
        Region {
            x: 99.,
            y: 0.,
            width: 2.,
            height: 1.
        }
        .validate(100., 100.)
        .is_err()
    );
}
