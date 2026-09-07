//! Real pool threads with a fake processor: no VA display, codec or GPU calls.

use std::{sync::Condvar, thread::ThreadId, time::Duration};
use weld_media::{EncodedFrameKind, VideoCodec};

use super::*;

#[derive(Default)]
struct State {
    created: usize,
    started: Vec<(u64, MediaStreamId, ThreadId)>,
    released: HashSet<u64>,
    release_all: bool,
    fail_init: bool,
    panic_tokens: HashSet<u64>,
    native: HashSet<(ThreadId, GenerationKey)>,
    max_native: usize,
    retired: Vec<GenerationKey>,
}

#[derive(Default)]
struct Control {
    state: Mutex<State>,
    changed: Condvar,
}

struct FakeProcessor {
    control: Arc<Control>,
    owner: ThreadId,
}

impl Processor for FakeProcessor {
    fn decode(&mut self, request: VaapiDecodeRequest) -> Result<Vec<VaapiDecodedFrame>> {
        let key = (
            request.access_unit.frame.stream,
            request.access_unit.frame.generation,
        );
        let mut state = self.control.state.lock().expect("state");
        state.native.insert((self.owner, key));
        state.max_native = state.max_native.max(state.native.len());
        state.started.push((request.token, key.0, self.owner));
        let should_panic = state.panic_tokens.contains(&request.token);
        self.control.changed.notify_all();
        if should_panic {
            drop(state);
            panic!("fake worker panic");
        }
        let (state, timeout) = self
            .control
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |state| {
                !state.release_all && !state.released.contains(&request.token)
            })
            .expect("wait");
        ensure!(
            !timeout.timed_out(),
            "fake processor timed out waiting for test release"
        );
        drop(state);
        Ok(Vec::new())
    }

    fn retire(&mut self, key: GenerationKey) {
        let mut state = self.control.state.lock().expect("state");
        state.native.remove(&(self.owner, key));
        state.retired.push(key);
        self.control.changed.notify_all();
    }
}

impl Drop for FakeProcessor {
    fn drop(&mut self) {
        self.control
            .state
            .lock()
            .expect("state")
            .native
            .retain(|(owner, _)| *owner != self.owner);
    }
}

struct Fixture {
    pool: VaapiDecodeWorker,
    control: Arc<Control>,
    wakes: Receiver<()>,
}

impl Fixture {
    fn new(limits: DecodePoolLimits) -> Self {
        let control = Arc::new(Control::default());
        let factory_control = control.clone();
        let (sender, wakes) = mpsc::channel();
        let pool = VaapiDecodeWorker::with_factory(
            limits,
            Arc::new(move || {
                let mut state = factory_control.state.lock().expect("state");
                state.created += 1;
                ensure!(!state.fail_init, "fake render node unavailable");
                drop(state);
                Ok(Box::new(FakeProcessor {
                    control: factory_control.clone(),
                    owner: thread::current().id(),
                }))
            }),
            Arc::new(move || {
                let _ = sender.send(());
            }),
        );
        Self {
            pool,
            control,
            wakes,
        }
    }

    fn started(&self, count: usize) {
        let state = self.control.state.lock().expect("state");
        let (_state, timeout) = self
            .control
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |state| {
                state.started.len() < count
            })
            .expect("wait");
        assert!(!timeout.timed_out(), "workers did not start concurrently");
    }

    fn release(&self, tokens: &[u64]) {
        self.control
            .state
            .lock()
            .expect("state")
            .released
            .extend(tokens);
        self.control.changed.notify_all();
    }

    fn take(&mut self, count: usize) -> Vec<VaapiDecodeCompletion> {
        let mut output = Vec::new();
        while output.len() < count {
            let (completed, failure) = self.pool.drain();
            assert!(failure.is_none(), "{failure:?}");
            output.extend(completed);
            if output.len() < count {
                self.wakes
                    .recv_timeout(Duration::from_secs(2))
                    .expect("completion wake");
            }
        }
        output
    }

    fn submit(
        &mut self,
        mut request: VaapiDecodeRequest,
        completed: &mut Vec<VaapiDecodeCompletion>,
    ) {
        loop {
            match self.pool.try_decode(request) {
                Ok(()) => return,
                Err(VaapiWorkerSubmitError::Busy(pending)) => request = *pending,
                Err(error) => panic!("unexpected submission failure: {error:?}"),
            }
            let generations = self.pool.generations.len();
            let (output, failure) = self.pool.drain();
            assert!(failure.is_none(), "{failure:?}");
            let progressed = !output.is_empty() || generations != self.pool.generations.len();
            completed.extend(output);
            if !progressed {
                self.wakes
                    .recv_timeout(Duration::from_secs(2))
                    .expect("capacity wake");
            }
        }
    }

    fn retire_to(&mut self, count: usize) {
        while self.pool.generations.len() != count {
            let (completed, failure) = self.pool.drain();
            assert!(completed.is_empty());
            assert!(failure.is_none(), "{failure:?}");
            if self.pool.generations.len() != count {
                self.wakes
                    .recv_timeout(Duration::from_secs(2))
                    .expect("retirement wake");
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Runs before the pool field's Drop, including on a failed assertion.
        self.control.state.lock().expect("state").release_all = true;
        self.control.changed.notify_all();
    }
}

fn request(token: u64, stream: u64, generation: u64) -> VaapiDecodeRequest {
    VaapiDecodeRequest {
        token,
        access_unit: EncodedAccessUnit {
            frame: MediaFrameId::new(
                MediaStreamId::new(stream),
                StreamGeneration::new(generation),
                token,
            ),
            codec: VideoCodec::Av1,
            kind: EncodedFrameKind::Keyframe,
            timestamp_micros: token,
            payload: vec![1, 2, 3],
        },
        visible_width: 1,
        visible_height: 1,
        xrgb_modifiers: vec![0],
    }
}

fn key(stream: u64, generation: u64) -> GenerationKey {
    (
        MediaStreamId::new(stream),
        StreamGeneration::new(generation),
    )
}

#[test]
fn limits_reject_empty_or_inconsistent_execution_budgets() {
    assert!(DecodePoolLimits::try_new(0, 1, 16).is_err());
    assert!(DecodePoolLimits::try_new(2, 3, 16).is_err());
    assert!(DecodePoolLimits::try_new(4, 2, 3).is_err());
    assert!(DecodePoolLimits::try_new(4, 2, 16).is_ok());
}

#[test]
fn lazy_growth_is_bounded_and_independent_streams_complete_out_of_order() {
    let mut fixture = Fixture::new(DecodePoolLimits::try_new(3, 3, 6).expect("limits"));
    for stream in 1..=100 {
        fixture
            .pool
            .retire(key(stream, 1).0, key(stream, 1).1)
            .expect("unknown retirement");
    }
    assert!(fixture.pool.workers.is_empty());
    assert!(fixture.pool.events.try_recv().is_err());
    for stream in 1..=3 {
        fixture
            .pool
            .try_decode(request(stream, stream, 1))
            .expect("admit");
    }
    fixture.started(3); // None can finish until explicitly released.
    assert_eq!(fixture.pool.workers.len(), 3);
    let fourth = request(4, 4, 1);
    let pointer = fourth.access_unit.payload.as_ptr();
    let fourth = match fixture.pool.try_decode(fourth) {
        Err(VaapiWorkerSubmitError::Busy(request)) => request,
        _ => panic!("job cap must return Busy"),
    };
    assert_eq!(fourth.access_unit.payload.as_ptr(), pointer);
    fixture.release(&[3]);
    assert_eq!(fixture.take(1)[0].token, 3);
    assert!(matches!(
        fixture.pool.try_decode(request(5, 1, 1)),
        Err(VaapiWorkerSubmitError::Busy(_))
    ));
    fixture
        .pool
        .try_decode(*fourth)
        .expect("reuse free worker at cap");
    fixture.started(4);
    fixture.release(&[1, 2, 4]);
    assert_eq!(fixture.take(3).len(), 3);
    assert_eq!(fixture.control.state.lock().expect("state").created, 3);
}

#[test]
fn retirement_is_idempotent_and_waits_for_active_jobs_before_releasing_capacity() {
    let mut fixture = Fixture::new(DecodePoolLimits::try_new(1, 1, 1).expect("limits"));
    fixture.pool.try_decode(request(1, 1, 1)).expect("admit");
    fixture
        .pool
        .retire(key(1, 1).0, key(1, 1).1)
        .expect("retire queued or active job");
    fixture
        .pool
        .retire(key(1, 1).0, key(1, 1).1)
        .expect("duplicate");
    fixture.started(1);
    assert!(
        fixture
            .control
            .state
            .lock()
            .expect("state")
            .retired
            .is_empty()
    );
    assert!(!fixture.pool.generations[&key(1, 1)].retirement_sent);
    fixture.release(&[1]);
    fixture.take(1);
    assert!(matches!(
        fixture.pool.try_decode(request(2, 1, 2)),
        Err(VaapiWorkerSubmitError::Busy(_))
    ));
    fixture
        .pool
        .retire(key(1, 1).0, key(1, 1).1)
        .expect("duplicate pending ACK");
    fixture.retire_to(0);
    assert_eq!(
        fixture.control.state.lock().expect("state").retired,
        vec![key(1, 1)]
    );
    fixture.submit(request(2, 1, 2), &mut Vec::new());
    fixture.started(2);
    fixture.release(&[2]);
    fixture.take(1);
    let state = fixture.control.state.lock().expect("state");
    assert_eq!(state.created, 1, "parked worker reused");
    assert_eq!(state.started[0].2, state.started[1].2, "stream affinity");
}

#[test]
fn generation_budget_is_shared_across_workers_and_rotations_wait_for_ack() {
    let mut fixture = Fixture::new(DecodePoolLimits::default());
    fixture.control.state.lock().expect("state").release_all = true;
    for generation in 1..=3 {
        let mut completed = Vec::new();
        if generation > 1 {
            for stream in 1..=16 {
                fixture
                    .pool
                    .retire(key(stream, generation - 1).0, key(stream, generation - 1).1)
                    .expect("retire");
            }
            assert!(matches!(
                fixture
                    .pool
                    .try_decode(request(generation * 100, 1, generation)),
                Err(VaapiWorkerSubmitError::Busy(_))
            ));
        }
        for stream in 1..=16 {
            fixture.submit(
                request(generation * 100 + stream, stream, generation),
                &mut completed,
            );
        }
        if completed.len() < 16 {
            completed.extend(fixture.take(16 - completed.len()));
        }
        assert_eq!(completed.len(), 16);
        assert_eq!(fixture.pool.generations.len(), 16);
        assert!(fixture.control.state.lock().expect("state").max_native <= 16);
    }
    assert_eq!(fixture.control.state.lock().expect("state").created, 4);
}

#[test]
fn full_wake_channel_still_retires_idle_keys_after_the_active_decode() {
    let mut fixture = Fixture::new(DecodePoolLimits::try_new(1, 1, 2).expect("limits"));
    fixture
        .pool
        .try_decode(request(1, 1, 1))
        .expect("first context");
    fixture.release(&[1]);
    fixture.take(1);
    fixture
        .pool
        .try_decode(request(2, 2, 1))
        .expect("second context");
    fixture.started(2);
    assert!(
        fixture.pool.workers[0]
            .commands
            .as_ref()
            .expect("sender")
            .try_send(Command::Wake)
            .is_ok()
    );
    fixture
        .pool
        .retire(key(1, 1).0, key(1, 1).1)
        .expect("Wake Full is covered");
    fixture.release(&[2]);
    fixture.take(1);
    fixture.retire_to(1);
    assert_eq!(
        fixture.control.state.lock().expect("state").retired,
        vec![key(1, 1)]
    );
}

#[test]
fn initialization_failure_is_sticky_and_keeps_original_diagnostic() {
    let mut fixture = Fixture::new(DecodePoolLimits::try_new(1, 1, 1).expect("limits"));
    fixture.control.state.lock().expect("state").fail_init = true;
    fixture
        .pool
        .try_decode(request(1, 1, 1))
        .expect("lazy admission");
    fixture
        .wakes
        .recv_timeout(Duration::from_secs(2))
        .expect("failure wake");
    let (completed, failure) = fixture.pool.drain();
    assert_eq!(completed.len(), 1);
    assert!(
        format!("{:#}", failure.expect("terminal error")).contains("fake render node unavailable")
    );
    for token in 2..10 {
        let error = fixture
            .pool
            .try_decode(request(token, 1, 1))
            .expect_err("stopped pool");
        assert!(format!("{error:?}").contains("fake render node unavailable"));
    }
    assert_eq!(fixture.control.state.lock().expect("state").created, 1);
}

#[test]
fn worker_panic_does_not_hide_another_workers_completion() {
    let mut fixture = Fixture::new(DecodePoolLimits::try_new(2, 2, 2).expect("limits"));
    {
        let mut state = fixture.control.state.lock().expect("state");
        state.panic_tokens.insert(1);
        state.release_all = true;
    }
    fixture.pool.try_decode(request(1, 1, 1)).expect("first");
    fixture.pool.try_decode(request(2, 2, 1)).expect("second");
    let mut output = Vec::new();
    let mut failure = None;
    while output.len() < 2 || failure.is_none() {
        let (completed, terminal) = fixture.pool.drain();
        output.extend(completed);
        failure = failure.or(terminal);
        if output.len() < 2 || failure.is_none() {
            fixture
                .wakes
                .recv_timeout(Duration::from_secs(2))
                .expect("wake");
        }
    }
    assert!(
        output
            .iter()
            .any(|completion| completion.token == 1 && completion.result.is_err())
    );
    assert!(
        output
            .iter()
            .any(|completion| completion.token == 2 && completion.result.is_ok())
    );
    assert!(
        fixture
            .pool
            .workers
            .iter()
            .all(|worker| worker.job.is_none())
    );
}

#[test]
fn completed_but_undrained_jobs_stay_charged_and_shutdown_never_waits_for_a_consumer() {
    let mut fixture = Fixture::new(DecodePoolLimits::try_new(2, 2, 2).expect("limits"));
    fixture.control.state.lock().expect("state").release_all = true;
    fixture.pool.try_decode(request(1, 1, 1)).expect("first");
    fixture.pool.try_decode(request(2, 2, 1)).expect("second");
    for _ in 0..2 {
        fixture
            .wakes
            .recv_timeout(Duration::from_secs(2))
            .expect("completion queued");
    }
    assert!(matches!(
        fixture.pool.try_decode(request(3, 1, 1)),
        Err(VaapiWorkerSubmitError::Busy(_))
    ));
    let control = fixture.control.clone();
    drop(fixture);
    assert!(control.state.lock().expect("state").native.is_empty());
}
