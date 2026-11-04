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
    cursors: Option<Rc<RefCell<Vec<Option<weld_client::CompositionCursor>>>>>,
}
impl EncodeBackend for Encoder {
    fn configure_stream_mode(&mut self, mode: weld_client::SurfaceStreamMode) -> Result<()> {
        assert_eq!(mode, weld_client::SurfaceStreamMode::Composited);
        Ok(())
    }
    fn prepare_input(&self, lease: &ClientBufferLease) -> Result<PreparedEncodeInput> {
        let scene = lease.access::<ComposedBuffer>().expect("composition");
        self.sizes.borrow_mut().push(scene.extent);
        if let Some(cursors) = &self.cursors {
            cursors.borrow_mut().push(scene.cursor.clone());
        }
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
        cursor: None,
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
        cursors: None,
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
            include_cursor: false,
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
        include_cursor: false,
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
        include_cursor: false,
        duration_ms: 600_000,
        codec: Codec::Av1,
        region: None,
        after_sequence: None,
    };
    assert!(options.validate().is_ok());
    assert!(
        Options {
            include_cursor: false,
            duration_ms: 600_001,
            ..options.clone()
        }
        .validate()
        .is_err()
    );
    let (wake, awakened) = mpsc::channel();
    let encoder = Encoder {
        completed: vec![],
        cursors: None,
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

fn cursor_commit(x: f32, image: weld_client::ClientCursor) -> ClientSurfaceCommit {
    let mut frame = commit(10).into_state();
    for update in &mut frame.buffers {
        if let SurfaceBufferChange::Replaced { buffer, metadata } = &mut update.change {
            let mut scene = buffer.access::<ComposedBuffer>().expect("scene").clone();
            scene.cursor = Some(weld_client::CompositionCursor {
                image: image.clone(),
                position: weld_client::LogicalPoint::new(x, 10.),
            });
            *buffer = ClientBufferLease::new(
                buffer.buffer(),
                buffer.use_id(),
                *metadata,
                Rc::new(scene),
                |_| {},
            )
            .expect("cursor frame");
        }
    }
    frame.into()
}

#[test]
fn cursor_only_updates_toggle_without_commits_and_events_retain_acknowledged_times() {
    let now = Instant::now();
    let cursors = Rc::new(RefCell::new(Vec::new()));
    let (wake, awakened) = mpsc::channel();
    let surface = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 1), 1);
    let options: Options = serde_json::from_str(
        r#"{"duration_ms":100,"region":{"x":5,"y":5,"width":30,"height":20}}"#,
    )
    .expect("defaults");
    assert!(!options.include_cursor, "cursor is opt-in");
    let mut recorder = Recorder::new(
        "cursor".into(),
        surface,
        cursor_commit(10., Default::default()),
        options,
        Box::new(Encoder {
            completed: vec![],
            cursors: Some(cursors.clone()),
            payload_bytes: 1,
            sizes: Rc::new(RefCell::new(vec![])),
        }),
        now,
        Arc::new(move || {
            let _ = wake.send(());
        }),
    )
    .expect("recorder");
    awakened
        .recv_timeout(Duration::from_secs(5))
        .expect("initial write");
    for (millis, enabled, x, hidden, emitted) in [
        (10, true, 10., false, true),
        (20, true, 20., false, true),
        (30, false, 20., false, true),
        (40, false, 30., false, false),
        (50, true, 30., true, false),
        (60, true, 30., false, true),
        (70, true, 30., true, true),
    ] {
        recorder
            .set_include_cursor(enabled, now + Duration::from_millis(millis))
            .expect("active toggle");
        let image = if hidden {
            weld_client::ClientCursor::Hidden
        } else {
            Default::default()
        };
        let time = now + Duration::from_millis(millis);
        recorder
            .observe_cursor(&cursor_commit(x, image), time)
            .expect("cursor-only observation");
        recorder.tick(time).expect("encode");
        if emitted {
            awakened
                .recv_timeout(Duration::from_secs(5))
                .expect("cursor encoded despite unchanged client revision");
        }
    }
    let action = serde_json::json!(["m", 20, 10, 20, "ease_in_out"]);
    recorder
        .observe_input(
            2,
            &action,
            now + Duration::from_millis(10),
            false,
            Some("10".into()),
        )
        .expect("first acknowledgement");
    recorder
        .observe_input(
            2,
            &action,
            now + Duration::from_millis(30),
            true,
            Some("10".into()),
        )
        .expect("completion");
    recorder
        .observe_input(
            4,
            &serde_json::json!(["p", 57, 100]),
            now + Duration::from_millis(40),
            false,
            None,
        )
        .expect("partial input");
    assert!(
        recorder.info().events.is_empty(),
        "poll summaries do not copy event payloads"
    );
    assert_eq!(recorder.timeline().events().expect("live events").len(), 2);
    drain(&mut recorder, now + Duration::from_millis(100), &awakened);
    assert!(
        recorder
            .set_include_cursor(true, now + Duration::from_millis(100))
            .is_err(),
        "completed recording cannot be toggled"
    );
    let clip = recorder.finish().expect("clip");
    assert_eq!(clip.info.submitted_frames, 6);
    assert_eq!(clip.info.frame_count, 6);
    assert!(clip.info.frames.iter().all(|frame| frame.revision == "10"));
    let samples = cursors.borrow();
    assert_eq!(samples.len(), 6);
    assert!(samples[0].is_none());
    assert_eq!(
        samples[1].as_ref().expect("visible").position,
        weld_client::LogicalPoint::new(5., 5.)
    );
    assert_eq!(
        samples[2].as_ref().expect("moved").position,
        weld_client::LogicalPoint::new(15., 5.)
    );
    assert!(samples[3].is_none() && samples[5].is_none());
    assert_eq!(clip.info.event_count, 2);
    assert_eq!(
        (
            clip.info.events[0].first_time_us,
            clip.info.events[0].time_us
        ),
        (10_000, 30_000)
    );
    assert!(clip.info.events[0].complete);
    assert!(!clip.info.events[1].complete);
}
