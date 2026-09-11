//! Demand-driven, stream-affine decode workers. Native contexts stay on their
//! creating thread. The coordinator bounds accepted jobs and owned generations,
//! including contexts awaiting retirement acknowledgement. Idle threads park;
//! their codec contexts retire promptly without needing another frame.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use anyhow::{Context, Result, ensure};

use crate::{
    DecodePipelineTiming, DecodeTiming, MediaFrameId, MediaStreamId, StreamGeneration,
    WorkerSubmitError,
};

type GenerationKey = (MediaStreamId, StreamGeneration);
type Notifier = Arc<dyn Fn() + Send + Sync>;
type Factory<P> = Arc<dyn Fn() -> Result<P> + Send + Sync>;

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
    /// Maximum outstanding jobs per worker, including undrained completions.
    pub fn depth(self) -> usize {
        self.depth
    }

    /// Validate positive limits with jobs <= workers * depth and workers <=
    /// generations. The current low-delay pipeline supports depth one or two.
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

/// Identity used for admission and retirement; payload and native target stay
/// backend-owned. Both identities must remain stable for the request lifetime.
pub trait DecodeJob: Send + 'static {
    /// Caller-assigned job token, unique among outstanding jobs.
    fn token(&self) -> u64;
    /// Encoded frame identity, including its stream and generation.
    fn frame(&self) -> MediaFrameId;
}

/// One completed job and its local timing, independent of output representation.
pub struct DecodeCompletion<O> {
    /// The submitted job's unchanged token.
    pub token: u64,
    /// Backend-owned outputs, or the failure of this job.
    pub result: Result<Vec<O>>,
    /// Absent for jobs failed before native execution could report timing.
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

struct Worker<R> {
    commands: Option<SyncSender<Command<R>>>,
    retirements: Arc<RetirementMailbox>,
    jobs: VecDeque<Job>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct RetirementMailbox {
    keys: Mutex<HashSet<GenerationKey>>,
    wake_queued: AtomicBool,
}

enum Command<R> {
    Decode(R, Instant),
    Wake,
}

enum Event<O> {
    Decoded(usize, DecodeCompletion<O>),
    Retired(usize, GenerationKey),
    Failed(usize, anyhow::Error),
}

/// Worker-local codec execution. Constructed, called and dropped on one worker;
/// the processor itself need not be Send. Only requests and output values cross
/// threads. Native output ownership and synchronization belong to the backend.
pub trait DecodeProcessor: 'static {
    /// The backend's owned request, including any native target configuration.
    type Request: DecodeJob;
    /// Completed outputs must own storage/leases that remain valid and immutable
    /// across later submissions, generation retirement and processor destruction.
    type Output: Send + 'static;

    /// Occupy one FIFO slot, including on submission failure. The pool calls
    /// [`Self::complete`] exactly once for each slot unless shutdown or a panic
    /// aborts it.
    fn submit(&mut self, request: Self::Request);
    /// Finish the oldest submitted job, returning zero or more outputs.
    ///
    /// May block, but must finish in bounded time without needing a future submit.
    /// The pool never polls an idle processor: retaining output past the final
    /// job until another input arrives would strand an idle stream. This is a
    /// low-delay execution contract, not support for arbitrary buffered codecs.
    /// Hardware calls that hang cannot be cancelled by this pool.
    fn complete(&mut self) -> Result<Vec<Self::Output>>;
    /// Release this generation only after all its jobs have completed and been
    /// drained by the caller. Must not invalidate previously returned outputs.
    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration);
}

/// Nonblocking facade over lazily allocated workers with persistent stream
/// affinity. Busy never consumes a request. Outputs own their allocations and
/// remain valid after the decoder generation retires.
pub struct DecodePool<P: DecodeProcessor> {
    limits: DecodePoolLimits,
    factory: Factory<P>,
    notify: Notifier,
    workers: Vec<Worker<P::Request>>,
    generations: HashMap<GenerationKey, Generation>,
    events: Receiver<Event<P::Output>>,
    event_sender: Sender<Event<P::Output>>,
    failure: Option<String>,
    terminal_failure: Option<anyhow::Error>,
}

impl<P: DecodeProcessor> DecodePool<P> {
    /// Create an empty pool. The factory runs lazily on each new worker, never
    /// on the caller. Notifications can run on any worker and must not block.
    /// Native admission limits remain the backend/host's responsibility.
    pub fn new(
        limits: DecodePoolLimits,
        factory: impl Fn() -> Result<P> + Send + Sync + 'static,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        // At most max_jobs decoded results + max_generations retirement ACKs
        // + max_workers one-shot failures can wait here. Wake receipt calls the
        // notifier DIRECTLY and must never enqueue an Event.
        let (event_sender, events) = mpsc::channel();
        Self {
            limits,
            factory: Arc::new(factory),
            notify: Arc::new(notify),
            workers: Vec::new(),
            generations: HashMap::new(),
            events,
            event_sender,
            failure: None,
            terminal_failure: None,
        }
    }

    /// Admit without blocking, preserving caller-provided stream order. Busy
    /// returns the original request; subsequent completion/retirement/failure
    /// notifications let the caller drain and retry. The caller must retire
    /// obsolete generations to reclaim their reservations. Capacity exhaustion
    /// alone does not schedule eviction or guarantee another wake.
    pub fn try_decode(&mut self, request: P::Request) -> Result<(), WorkerSubmitError<P::Request>> {
        if let Some(reason) = &self.failure {
            return Err(WorkerSubmitError::Rejected(anyhow::anyhow!(reason.clone())));
        }
        let key = (request.frame().stream, request.frame().generation);
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
            return Err(WorkerSubmitError::Busy(Box::new(request)));
        }
        let assigned = self
            .generations
            .iter()
            .find_map(|(owned, generation)| (owned.0 == key.0).then_some(generation.worker));
        let worker = match assigned {
            Some(worker) => worker,
            None => match self.assign_worker() {
                Ok(Some(worker)) => worker,
                Ok(None) => return Err(WorkerSubmitError::Busy(Box::new(request))),
                Err(error) => {
                    self.fail(error);
                    return Err(WorkerSubmitError::Rejected(anyhow::anyhow!(
                        self.failure.clone().unwrap_or_default()
                    )));
                }
            },
        };
        if self.workers[worker].jobs.len() >= self.limits.depth {
            return Err(WorkerSubmitError::Busy(Box::new(request)));
        }
        let job = Job {
            token: request.token(),
            frame: request.frame(),
        };
        let Some(sender) = &self.workers[worker].commands else {
            return Err(WorkerSubmitError::Stopped(Box::new(request)));
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
                Err(WorkerSubmitError::Busy(Box::new(request)))
            }
            // The worker's terminal event carries the original error. It can
            // race channel closure; keep ownership until drain reports it.
            Err(TrySendError::Disconnected(Command::Decode(request, _))) => {
                Err(WorkerSubmitError::Busy(Box::new(request)))
            }
            Err(_) => Err(WorkerSubmitError::Rejected(anyhow::anyhow!(
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
                .name(format!("weld-decode-{index}"))
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
                        Err(_) => Some(anyhow::anyhow!("decoder worker panicked")),
                    };
                    if let Some(error) = error {
                        let _ = events.send(Event::Failed(index, error));
                        notify();
                    }
                })
                .context("could not spawn decoder worker")?;
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
    pub fn drain(&mut self) -> (Vec<DecodeCompletion<P::Output>>, Option<anyhow::Error>) {
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
                            completions.push(DecodeCompletion {
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

impl<P: DecodeProcessor> Drop for DecodePool<P> {
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
                tracing::error!("decoder worker panicked during shutdown");
            }
        }
    }
}

fn worker_loop<P: DecodeProcessor>(
    index: usize,
    commands: Receiver<Command<P::Request>>,
    retirements: &RetirementMailbox,
    events: &Sender<Event<P::Output>>,
    notify: &Notifier,
    factory: &Factory<P>,
    depth: usize,
) -> Result<()> {
    let mut processor = factory().context("could not initialize decoder worker")?;
    let mut pending = VecDeque::new();
    let mut closed = false;
    loop {
        apply_retirements(index, &mut processor, retirements, events, notify)?;
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
            apply_retirements(index, &mut processor, retirements, events, notify)?;
            match command {
                Command::Wake => notify(),
                Command::Decode(request, queued_at) => {
                    let token = request.token();
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
        // Retained native work can be released without preparing unused outputs.
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
                    DecodeCompletion {
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

fn apply_retirements<P: DecodeProcessor>(
    index: usize,
    processor: &mut P,
    retirements: &RetirementMailbox,
    events: &Sender<Event<P::Output>>,
    notify: &Notifier,
) -> Result<()> {
    let keys = retirements
        .keys
        .lock()
        .map_err(|_| anyhow::anyhow!("decoder retirement lock poisoned"))?
        .drain()
        .collect::<Vec<_>>();
    for key in keys {
        processor.retire(key.0, key.1);
        events
            .send(Event::Retired(index, key))
            .map_err(|_| anyhow::anyhow!("decoder completion receiver closed"))?;
        notify();
    }
    Ok(())
}

#[cfg(test)]
mod tests;
