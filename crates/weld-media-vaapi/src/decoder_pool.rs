//! Demand-driven, stream-affine decode workers. Native contexts stay on their
//! creating thread. The coordinator bounds accepted jobs and owned generations,
//! including contexts awaiting retirement acknowledgement. Idle threads park;
//! their codec contexts retire promptly without needing another frame.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use anyhow::{Context, Result, ensure};
use weld_media::{
    DecodePipelineTiming, DecodeTiming, EncodedAccessUnit, MediaFrameId, MediaStreamId,
    StreamGeneration, VideoCodec,
};

use crate::{
    FfmpegDecoder, FfmpegVaapiDevice, PendingDecodedFrame, VaapiDevice, VaapiDmabuf,
    VaapiWorkerSubmitError, VppConverter,
};

type GenerationKey = (MediaStreamId, StreamGeneration);
type Notifier = Arc<dyn Fn() + Send + Sync>;
type Factory = Arc<dyn Fn() -> Result<Box<dyn Processor>> + Send + Sync>;

/// Conservative execution limits for ONE connection, not calibrated hardware
/// capacity. Multiple connections multiply these limits. `max_jobs` can reduce
/// concurrency below workers * depth. Completed but undrained jobs stay charged.
#[derive(Clone, Copy, Debug)]
pub struct DecodePoolLimits {
    max_workers: usize,
    max_jobs: usize,
    max_generations: usize,
    depth: usize,
}

impl DecodePoolLimits {
    pub fn try_new(
        max_workers: usize,
        max_jobs: usize,
        max_generations: usize,
        depth: usize,
    ) -> Result<Self> {
        ensure!(
            max_workers > 0 && max_jobs > 0 && max_generations > 0,
            "decoder pool limits must be positive"
        );
        ensure!(
            (1..=2).contains(&depth),
            "decoder pipeline depth must be one or two"
        );
        let capacity = max_workers
            .checked_mul(depth)
            .context("decoder job capacity overflow")?;
        ensure!(
            max_jobs <= capacity && max_workers <= max_generations,
            "decoder pool requires jobs <= workers * depth and workers <= generations"
        );
        Ok(Self {
            max_workers,
            max_jobs,
            max_generations,
            depth,
        })
    }
}

impl Default for DecodePoolLimits {
    fn default() -> Self {
        Self {
            max_workers: 4,
            max_jobs: 8,
            max_generations: 16,
            depth: 2,
        }
    }
}

pub struct VaapiDecodeRequest {
    pub token: u64,
    pub access_unit: EncodedAccessUnit,
    pub visible_width: u32,
    pub visible_height: u32,
    pub xrgb_modifiers: Vec<u64>,
}

pub struct VaapiDecodedFrame {
    pub frame: MediaFrameId,
    pub dmabuf: VaapiDmabuf,
}

pub struct VaapiDecodeCompletion {
    pub token: u64,
    pub result: Result<Vec<VaapiDecodedFrame>>,
    pub timing: Option<DecodeTiming>,
}

struct Job {
    token: u64,
    frame: MediaFrameId,
}

struct Generation {
    worker: usize,
    retiring: bool,
    retirement_sent: bool,
}

struct Worker {
    commands: Option<SyncSender<Command>>,
    retirements: Arc<RetirementMailbox>,
    jobs: VecDeque<Job>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct RetirementMailbox {
    keys: Mutex<HashSet<GenerationKey>>,
    wake_queued: AtomicBool,
}

enum Command {
    Decode(VaapiDecodeRequest, Instant),
    Wake,
}

enum Event {
    Decoded(usize, VaapiDecodeCompletion),
    Retired(usize, GenerationKey),
    Failed(usize, anyhow::Error),
}

trait Processor {
    // Every submission occupies one FIFO slot, including failed submissions.
    fn submit(&mut self, request: VaapiDecodeRequest);
    fn complete(&mut self) -> Result<Vec<VaapiDecodedFrame>>;
    fn retire(&mut self, key: GenerationKey);
}

/// Nonblocking facade over lazily allocated workers with persistent stream
/// affinity. Busy never consumes a request. Outputs own their allocations and
/// remain valid after the decoder generation retires.
pub struct VaapiDecodeWorker {
    limits: DecodePoolLimits,
    factory: Factory,
    notify: Notifier,
    workers: Vec<Worker>,
    generations: HashMap<GenerationKey, Generation>,
    events: Receiver<Event>,
    event_sender: Sender<Event>,
    failure: Option<String>,
    terminal_failure: Option<anyhow::Error>,
}

impl VaapiDecodeWorker {
    pub fn spawn(render_node: PathBuf, notify: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        Self::with_limits(render_node, DecodePoolLimits::default(), notify)
    }

    pub fn with_limits(
        render_node: PathBuf,
        limits: DecodePoolLimits,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        Ok(Self::with_factory(
            limits,
            Arc::new(move || {
                let device = VaapiDevice::open(&render_node)?;
                Ok(Box::new(NativeProcessor {
                    vpp: device.vpp_converter()?,
                    device: FfmpegVaapiDevice::open(&render_node)?,
                    sessions: HashMap::new(),
                    pending: VecDeque::new(),
                    poisoned: HashMap::new(),
                    depth: limits.depth,
                }))
            }),
            Arc::new(notify),
        ))
    }

    fn with_factory(limits: DecodePoolLimits, factory: Factory, notify: Notifier) -> Self {
        // At most max_jobs decoded results + max_generations retirement ACKs
        // + max_workers one-shot failures can wait here. Wake receipt calls the
        // notifier DIRECTLY and must never enqueue an Event.
        let (event_sender, events) = mpsc::channel();
        Self {
            limits,
            factory,
            notify,
            workers: Vec::new(),
            generations: HashMap::new(),
            events,
            event_sender,
            failure: None,
            terminal_failure: None,
        }
    }

    pub fn try_decode(
        &mut self,
        request: VaapiDecodeRequest,
    ) -> Result<(), VaapiWorkerSubmitError<VaapiDecodeRequest>> {
        if let Some(reason) = &self.failure {
            return Err(VaapiWorkerSubmitError::Rejected(anyhow::anyhow!(
                reason.clone()
            )));
        }
        let key = (
            request.access_unit.frame.stream,
            request.access_unit.frame.generation,
        );
        if self
            .generations
            .get(&key)
            .is_some_and(|generation| generation.retiring)
            || self
                .workers
                .iter()
                .map(|worker| worker.jobs.len())
                .sum::<usize>()
                >= self.limits.max_jobs
            || (!self.generations.contains_key(&key)
                && self.generations.len() >= self.limits.max_generations)
        {
            return Err(VaapiWorkerSubmitError::Busy(Box::new(request)));
        }
        let assigned = self
            .generations
            .iter()
            .find_map(|(owned, generation)| (owned.0 == key.0).then_some(generation.worker));
        let worker = match assigned {
            Some(worker) => worker,
            None => match self.assign_worker() {
                Ok(Some(worker)) => worker,
                Ok(None) => return Err(VaapiWorkerSubmitError::Busy(Box::new(request))),
                Err(error) => {
                    self.fail(error);
                    return Err(VaapiWorkerSubmitError::Rejected(anyhow::anyhow!(
                        self.failure.clone().unwrap_or_default()
                    )));
                }
            },
        };
        if self.workers[worker].jobs.len() >= self.limits.depth {
            return Err(VaapiWorkerSubmitError::Busy(Box::new(request)));
        }
        let job = Job {
            token: request.token,
            frame: request.access_unit.frame,
        };
        let Some(sender) = &self.workers[worker].commands else {
            return Err(VaapiWorkerSubmitError::Stopped(Box::new(request)));
        };
        match sender.try_send(Command::Decode(request, Instant::now())) {
            Ok(()) => {
                self.workers[worker].jobs.push_back(job);
                let inserted = self
                    .generations
                    .insert(
                        key,
                        Generation {
                            worker,
                            retiring: false,
                            retirement_sent: false,
                        },
                    )
                    .is_none();
                if inserted {
                    self.report_ownership();
                }
                Ok(())
            }
            Err(TrySendError::Full(Command::Decode(request, _))) => {
                Err(VaapiWorkerSubmitError::Busy(Box::new(request)))
            }
            // The worker's terminal event carries the original error. It can
            // race channel closure; keep ownership until drain reports it.
            Err(TrySendError::Disconnected(Command::Decode(request, _))) => {
                Err(VaapiWorkerSubmitError::Busy(Box::new(request)))
            }
            Err(_) => Err(VaapiWorkerSubmitError::Rejected(anyhow::anyhow!(
                "decoder command channel returned another command"
            ))),
        }
    }

    fn assign_worker(&mut self) -> Result<Option<usize>> {
        let owned = (0..self.workers.len())
            .map(|worker| {
                self.generations
                    .values()
                    .filter(|generation| generation.worker == worker)
                    .count()
            })
            .collect::<Vec<_>>();
        if let Some(worker) = owned.iter().position(|count| *count == 0) {
            return Ok(Some(worker));
        }
        if self.workers.len() < self.limits.max_workers {
            let index = self.workers.len();
            // One additional slot is exclusively headroom for the coalesced Wake.
            let (sender, receiver) = mpsc::sync_channel(self.limits.depth + 1);
            let retirements = Arc::new(RetirementMailbox::default());
            let worker_retirements = retirements.clone();
            let events = self.event_sender.clone();
            let notify = self.notify.clone();
            let factory = self.factory.clone();
            let depth = self.limits.depth;
            let thread = thread::Builder::new()
                .name(format!("weld-vaapi-decode-{index}"))
                .spawn(move || {
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        worker_loop(
                            index,
                            receiver,
                            &worker_retirements,
                            &events,
                            &notify,
                            &factory,
                            depth,
                        )
                    }));
                    let error = match result {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(error),
                        Err(_) => Some(anyhow::anyhow!("VA-API decoder worker panicked")),
                    };
                    if let Some(error) = error {
                        let _ = events.send(Event::Failed(index, error));
                        notify();
                    }
                })
                .context("could not spawn VA-API decoder worker")?;
            self.workers.push(Worker {
                commands: Some(sender),
                retirements,
                jobs: VecDeque::new(),
                thread: Some(thread),
            });
            Ok(Some(index))
        } else {
            Ok(owned
                .iter()
                .enumerate()
                .filter(|(index, _)| self.workers[*index].jobs.len() < self.limits.depth)
                .min_by_key(|(_, count)| **count)
                .map(|(index, _)| index))
        }
    }

    /// Idempotent; a reservation remains charged until the worker acknowledges
    /// retirement, even if native decode previously failed or never created it.
    pub fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()> {
        if let Some(reason) = &self.failure {
            anyhow::bail!(reason.clone());
        }
        let key = (stream, generation);
        let Some(owned) = self.generations.get_mut(&key) else {
            return Ok(());
        };
        owned.retiring = true;
        self.send_retirements()
    }

    fn send_retirements(&mut self) -> Result<()> {
        for (&key, generation) in &mut self.generations {
            if !generation.retiring
                || generation.retirement_sent
                || self.workers[generation.worker]
                    .jobs
                    .iter()
                    .any(|job| (job.frame.stream, job.frame.generation) == key)
            {
                continue;
            }
            let worker = &self.workers[generation.worker];
            // Publish the set entry BEFORE signalling. Full means a queued
            // command guarantees a retirement check before the next recv wait.
            worker
                .retirements
                .keys
                .lock()
                .map_err(|_| anyhow::anyhow!("decoder retirement lock poisoned"))?
                .insert(key);
            generation.retirement_sent = true;
            // Publish under the Mutex before signalling. Worker clears this
            // SeqCst flag before locking/draining, so racing inserts cannot sleep.
            if worker.retirements.wake_queued.swap(true, Ordering::SeqCst) {
                continue;
            }
            let sender = worker
                .commands
                .as_ref()
                .context("decoder worker stopped before retirement")?;
            match sender.try_send(Command::Wake) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    // Defensive only: <= depth Decodes + one coalesced Wake fit.
                    worker
                        .retirements
                        .wake_queued
                        .store(false, Ordering::SeqCst);
                }
                // A terminal event is forthcoming; drain preserves its full
                // error rather than replacing it with a generic channel error.
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
        Ok(())
    }

    /// Returns every completion alongside a terminal failure, never dropping a
    /// successful result merely because another worker failed in the same batch.
    pub fn drain(&mut self) -> (Vec<VaapiDecodeCompletion>, Option<anyhow::Error>) {
        let mut completions = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Decoded(worker, completion) => {
                    let valid = self
                        .workers
                        .get(worker)
                        .and_then(|worker| worker.jobs.front())
                        .is_some_and(|job| job.token == completion.token);
                    if valid {
                        self.workers[worker].jobs.pop_front();
                        completions.push(completion);
                    } else {
                        self.fail(anyhow::anyhow!(
                            "decoder worker completed an unexpected token"
                        ));
                    }
                }
                Event::Retired(worker, key) => {
                    if self.generations.get(&key).is_some_and(|generation| {
                        generation.worker == worker
                            && generation.retiring
                            && generation.retirement_sent
                    }) {
                        self.generations.remove(&key);
                        self.report_ownership();
                    } else {
                        self.fail(anyhow::anyhow!(
                            "unexpected decoder retirement acknowledgement"
                        ));
                    }
                }
                Event::Failed(worker, error) => {
                    if let Some(worker) = self.workers.get_mut(worker) {
                        for job in worker.jobs.drain(..) {
                            completions.push(VaapiDecodeCompletion {
                                token: job.token,
                                result: Err(anyhow::anyhow!(format!("{error:#}"))),
                                timing: None,
                            });
                        }
                    }
                    self.fail(error);
                }
            }
        }
        if self.failure.is_none()
            && let Err(error) = self.send_retirements()
        {
            self.fail(error);
        }
        (completions, self.terminal_failure.take())
    }

    fn fail(&mut self, error: anyhow::Error) {
        if self.failure.is_none() {
            self.failure = Some(format!("{error:#}"));
            self.terminal_failure = Some(error);
        }
    }

    fn report_ownership(&self) {
        if !tracing::enabled!(target: "weld_media_diag", tracing::Level::DEBUG) {
            return;
        }
        let owned = (0..self.workers.len())
            .map(|worker| {
                let keys = self
                    .generations
                    .iter()
                    .filter(|(_, generation)| generation.worker == worker);
                let generations = keys.clone().count();
                let streams = keys.map(|(key, _)| key.0).collect::<HashSet<_>>().len();
                (worker, streams, generations)
            })
            .collect::<Vec<_>>();
        tracing::debug!(target: "weld_media_diag", workers = self.workers.len(),
            max_workers = self.limits.max_workers, max_jobs = self.limits.max_jobs, depth = self.limits.depth,
            generations = self.generations.len(), ?owned, "decoder pool ownership");
    }
}

impl Drop for VaapiDecodeWorker {
    fn drop(&mut self) {
        for worker in &mut self.workers {
            worker.commands.take();
        }
        // No worker can block sending a completion. Native calls still have to
        // finish; a GPU/driver hang is not made recoverable by this pool.
        for worker in &mut self.workers {
            if let Some(thread) = worker.thread.take()
                && thread.join().is_err()
            {
                tracing::error!("VA-API decoder worker panicked during shutdown");
            }
        }
    }
}

fn worker_loop(
    index: usize,
    commands: Receiver<Command>,
    retirements: &RetirementMailbox,
    events: &Sender<Event>,
    notify: &Notifier,
    factory: &Factory,
    depth: usize,
) -> Result<()> {
    let mut processor = factory().context("could not initialize VA-API decoder worker")?;
    let mut pending = VecDeque::new();
    let mut closed = false;
    loop {
        apply_retirements(index, &mut *processor, retirements, events, notify)?;
        while pending.len() < depth && !closed {
            let command = if pending.is_empty() {
                commands.recv().ok()
            } else {
                match commands.try_recv() {
                    Ok(command) => Some(command),
                    Err(TryRecvError::Empty) => break, // Never wait to fill a batch.
                    Err(TryRecvError::Disconnected) => None,
                }
            };
            let Some(command) = command else {
                closed = true;
                break;
            };
            if matches!(&command, Command::Wake) {
                retirements.wake_queued.store(false, Ordering::SeqCst);
            }
            apply_retirements(index, &mut *processor, retirements, events, notify)?;
            match command {
                Command::Wake => notify(),
                Command::Decode(request, queued_at) => {
                    let token = request.token;
                    let started_at = Instant::now();
                    processor.submit(request);
                    pending.push_back((
                        token,
                        queued_at,
                        started_at,
                        DecodePipelineTiming {
                            submitted_at: Instant::now(),
                            finishing_at: started_at,
                            had_pending_frame: !pending.is_empty(),
                        },
                    ));
                }
            }
        }
        // Closure means the owner is dropping and will never drain results.
        // Retained AVFrames can be released without allocating/converting XRGB.
        if closed {
            return Ok(());
        }
        if let Some((token, queued_at, started_at, mut pipeline)) = pending.pop_front() {
            pipeline.finishing_at = Instant::now();
            let result = processor.complete();
            let completed_at = Instant::now();
            if events
                .send(Event::Decoded(
                    index,
                    VaapiDecodeCompletion {
                        token,
                        result,
                        timing: Some(DecodeTiming {
                            queued_at,
                            started_at,
                            completed_at,
                            pipeline: Some(pipeline),
                        }),
                    },
                ))
                .is_err()
            {
                return Ok(());
            }
            notify();
        }
    }
}

fn apply_retirements(
    index: usize,
    processor: &mut dyn Processor,
    retirements: &RetirementMailbox,
    events: &Sender<Event>,
    notify: &Notifier,
) -> Result<()> {
    let keys = retirements
        .keys
        .lock()
        .map_err(|_| anyhow::anyhow!("decoder retirement lock poisoned"))?
        .drain()
        .collect::<Vec<_>>();
    for key in keys {
        processor.retire(key);
        events
            .send(Event::Retired(index, key))
            .context("decoder completion receiver closed")?;
        notify();
    }
    Ok(())
}

struct NativeProcessor {
    // Hardware frames are released before the codec/device owners on shutdown.
    pending: VecDeque<NativePending>,
    poisoned: HashMap<GenerationKey, String>,
    sessions: HashMap<GenerationKey, NativeSession>,
    vpp: VppConverter,
    device: FfmpegVaapiDevice,
    depth: usize,
}

struct NativePending {
    key: GenerationKey,
    frame: MediaFrameId,
    width: u32,
    height: u32,
    modifiers: Vec<u64>,
    result: Result<PendingDecodedFrame>,
}

struct NativeSession {
    decoder: FfmpegDecoder,
    codec: VideoCodec,
}

impl NativeProcessor {
    fn prepare(&mut self, request: &VaapiDecodeRequest) -> Result<PendingDecodedFrame> {
        let frame = request.access_unit.frame;
        let key = (frame.stream, frame.generation);
        if let Some(error) = self.poisoned.get(&key) {
            anyhow::bail!(error.clone());
        }
        if let std::collections::hash_map::Entry::Vacant(entry) = self.sessions.entry(key) {
            entry.insert(NativeSession {
                decoder: FfmpegDecoder::new(request.access_unit.codec, &self.device, self.depth)?,
                codec: request.access_unit.codec,
            });
        }
        let session = self
            .sessions
            .get_mut(&key)
            .context("decoder session disappeared")?;
        ensure!(
            session.codec == request.access_unit.codec,
            "encoded stream generation changed codec without retirement"
        );
        session.decoder.submit(
            &request.access_unit.payload,
            request.access_unit.timestamp_micros,
        )
    }
}

impl Processor for NativeProcessor {
    fn submit(&mut self, request: VaapiDecodeRequest) {
        let frame = request.access_unit.frame;
        let key = (frame.stream, frame.generation);
        let result = self.prepare(&request);
        if let Err(error) = &result {
            self.poisoned
                .entry(key)
                .or_insert_with(|| format!("{error:#}"));
        }
        self.pending.push_back(NativePending {
            key,
            frame,
            width: request.visible_width,
            height: request.visible_height,
            modifiers: request.xrgb_modifiers,
            result,
        });
    }

    fn complete(&mut self) -> Result<Vec<VaapiDecodedFrame>> {
        let pending = self
            .pending
            .pop_front()
            .context("native decoder FIFO is empty")?;
        let result = pending.result.and_then(|frame| {
            let decoded =
                frame.finish(pending.width, pending.height, &pending.modifiers, &self.vpp)?;
            ensure!(
                decoded.dmabuf.width == pending.width && decoded.dmabuf.height == pending.height,
                "decoded output extent differs from transported visible extent"
            );
            Ok(vec![VaapiDecodedFrame {
                frame: pending.frame,
                dmabuf: decoded.dmabuf,
            }])
        });
        if let Err(error) = &result {
            self.poisoned
                .entry(pending.key)
                .or_insert_with(|| format!("{error:#}"));
        }
        if self.poisoned.contains_key(&pending.key)
            && !self.pending.iter().any(|queued| queued.key == pending.key)
        {
            self.sessions.remove(&pending.key);
            self.poisoned.remove(&pending.key);
        }
        result
    }

    fn retire(&mut self, key: GenerationKey) {
        self.sessions.remove(&key);
        self.poisoned.remove(&key);
    }
}

#[cfg(test)]
mod tests;
