use anyhow::Result;
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, mpsc},
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
    payload_bytes: usize,
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
                payload: vec![1; self.payload_bytes],
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
        payload_bytes: 1,
        sizes: sizes.clone(),
    };
    let surface = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 1), 1);
    let (wake, awakened) = mpsc::channel();
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
        Arc::new(move || {
            let _ = wake.send(());
        }),
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
    drain(&mut recorder, now + Duration::from_millis(100), &awakened);
    let clip = recorder.finish().expect("stored clip");
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

fn drain(recorder: &mut Recorder, end: Instant, awakened: &mpsc::Receiver<()>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !recorder.tick(end).expect("drain recording") {
        assert!(Instant::now() < deadline, "storage did not finish");
        let _ = awakened.recv_timeout(Duration::from_millis(10));
    }
}

#[test]
fn ten_minute_recording_outlives_old_frame_and_duration_limits() {
    let now = Instant::now();
    let options = Options {
        duration_ms: 600_000,
        codec: Codec::Av1,
        region: None,
        after_sequence: None,
    };
    assert!(options.validate().is_ok());
    assert!(
        Options {
            duration_ms: 600_001,
            ..options.clone()
        }
        .validate()
        .is_err()
    );
    let (wake, awakened) = mpsc::channel();
    let encoder = Encoder {
        completed: vec![],
        payload_bytes: 1024,
        sizes: Rc::new(RefCell::new(Vec::new())),
    };
    let surface = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 1), 1);
    let mut recorder = Recorder::new(
        "long".into(),
        surface,
        commit(1),
        options,
        Box::new(encoder),
        now,
        Arc::new(move || {
            let _ = wake.send(());
        }),
    )
    .expect("recorder");
    // Exercise all 36,000 updates without a ten-minute wall-clock sleep. Await each
    // storage notification so this test does not benchmark temporary-file speed.
    for index in 1..=36_000_u64 {
        awakened
            .recv_timeout(Duration::from_secs(5))
            .expect("storage completion");
        let time = now + Duration::from_micros(index * 600_000_000 / 36_000);
        recorder.observe(&commit(index + 1), time).expect("observe");
        recorder.tick(time).expect("encode");
    }
    drain(&mut recorder, now + Duration::from_secs(600), &awakened);
    let clip = recorder.finish().expect("clip");
    assert!(clip.info.bytes > 32 * 1024 * 1024);
    assert_eq!(clip.info.frame_count, 36_001);
    assert_eq!(clip.info.dropped_frames, Some(0));
    assert_eq!(clip.info.frames.last().expect("last").time_us, 600_000_000);
    assert_eq!(clip.info.expires_after_idle_ms, 1_800_000);
    assert_eq!(
        clip.frames
            .last()
            .expect("packet")
            .unit
            .bytes()
            .expect("read"),
        vec![1; 1024]
    );
}
