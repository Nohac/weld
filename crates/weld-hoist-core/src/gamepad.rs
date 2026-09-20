//! One optional host-visible gamepad per authenticated connection. Surface
//! admission and keyboard/pointer focus grant no authority over this device.
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use crate::HoistPortResult;
pub use weld_hoist_protocol::gamepad::{
    GamepadButtons, GamepadCaptureState, GamepadRequest, GamepadState, GamepadStatus, HatDirection,
};

const WATCHDOG: Duration = Duration::from_millis(750);
const RENEWAL: Duration = Duration::from_millis(100);
const CAPACITY: usize = 64;

#[cfg(test)]
#[path = "gamepad_tests.rs"]
pub(crate) mod tests;

/// A native provider is injected only by trusted host assembly, never a peer.
pub trait GamepadProvider {
    fn open(&mut self) -> HoistPortResult<Box<dyn GamepadDevice>>;
}
/// Implementations must destroy their device on Drop, including after an error.
pub trait GamepadDevice {
    fn update(&mut self, state: GamepadState) -> HoistPortResult<()>;
}

struct Capture {
    generation: u64,
    deadline: Instant,
    device: Box<dyn GamepadDevice>,
}

#[derive(Default)]
pub(crate) struct GamepadSource {
    provider: Option<Box<dyn GamepadProvider>>,
    capture: Option<Capture>,
    floor: u64,
    advertised: bool,
}
impl GamepadSource {
    pub fn new(provider: Option<Box<dyn GamepadProvider>>) -> Self {
        Self {
            provider,
            capture: None,
            floor: 0,
            advertised: false,
        }
    }
    pub fn advertise(&mut self) -> Option<GamepadStatus> {
        if self.provider.is_some() && !self.advertised {
            self.advertised = true;
            Some(GamepadStatus::Available)
        } else {
            None
        }
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.capture.as_ref().map(|capture| capture.deadline)
    }
    pub fn expire(&mut self, now: Instant) -> Option<GamepadStatus> {
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            self.stop(GamepadCaptureState::TimedOut)
        } else {
            None
        }
    }
    pub fn stop(&mut self, state: GamepadCaptureState) -> Option<GamepadStatus> {
        let mut capture = self.capture.take()?;
        tracing::info!(
            generation = capture.generation,
            ?state,
            "remote gamepad capture ended"
        );
        if let Err(error) = capture.device.update(GamepadState::default()) {
            tracing::warn!(%error, "could not neutralize gamepad; destroying device");
        }
        Some(GamepadStatus::Capture {
            generation: capture.generation,
            state,
        })
    }
    pub fn accept(&mut self, request: GamepadRequest, now: Instant) -> Option<GamepadStatus> {
        let generation = request.generation();
        match request {
            GamepadRequest::Begin { .. } => {
                if generation <= self.floor {
                    return None;
                }
                self.floor = generation;
                self.stop(GamepadCaptureState::Stopped);
                let state = if let Some(provider) = &mut self.provider {
                    match provider.open() {
                        Ok(device) => {
                            tracing::info!(generation, "remote gamepad capture started");
                            self.capture = Some(Capture {
                                generation,
                                deadline: now + WATCHDOG,
                                device,
                            });
                            GamepadCaptureState::Active
                        }
                        Err(error) => {
                            tracing::warn!(%error, "could not create remote gamepad");
                            GamepadCaptureState::DeviceFailed
                        }
                    }
                } else {
                    GamepadCaptureState::Denied
                };
                Some(GamepadStatus::Capture { generation, state })
            }
            GamepadRequest::State { state, .. } => {
                let capture = self
                    .capture
                    .as_mut()
                    .filter(|capture| capture.generation == generation)?;
                if let Err(error) = capture.device.update(state) {
                    tracing::warn!(%error, "remote gamepad write failed");
                    return self.stop(GamepadCaptureState::DeviceFailed);
                }
                capture.deadline = now + WATCHDOG;
                None
            }
            GamepadRequest::End { .. } => {
                // An End may overtake a locally cancelled Begin. Retire its id
                // even when no native device was ever created.
                self.floor = self.floor.max(generation);
                if self
                    .capture
                    .as_ref()
                    .is_some_and(|capture| capture.generation <= generation)
                {
                    self.stop(GamepadCaptureState::Stopped)
                } else {
                    None
                }
            }
        }
    }
}
impl Drop for GamepadSource {
    fn drop(&mut self) {
        self.stop(GamepadCaptureState::Stopped);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GamepadMode {
    #[default]
    Unavailable,
    Ready,
    Pending(u64),
    Active(u64),
    Rejected(GamepadCaptureState),
    Disconnected,
}

struct Pending {
    mode: GamepadMode,
    generation: u64,
    queue: VecDeque<GamepadRequest>,
    end: Option<u64>,
    last_sample: Option<(GamepadState, Instant)>,
}
impl Default for Pending {
    fn default() -> Self {
        Self {
            mode: GamepadMode::Unavailable,
            generation: 0,
            queue: VecDeque::with_capacity(CAPACITY),
            end: None,
            last_sample: None,
        }
    }
}
impl Pending {
    fn stop(&mut self) {
        if let GamepadMode::Pending(generation) | GamepadMode::Active(generation) = self.mode {
            self.end = Some(generation);
            self.mode = GamepadMode::Ready;
        }
        self.queue.clear();
        self.last_sample = None;
    }
}

/// Cloneable main-thread producer. The relay consumes on its caller thread;
/// notifications always run after releasing the small control mutex.
#[derive(Clone)]
pub struct GamepadController {
    pending: Arc<Mutex<Pending>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}
impl Default for GamepadController {
    fn default() -> Self {
        Self::new(|| {})
    }
}
impl GamepadController {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            pending: Arc::default(),
            wake: Arc::new(wake),
        }
    }
    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    pub fn mode(&self) -> GamepadMode {
        self.lock().mode
    }
    /// Entry is explicit; a disconnected or unsupported peer never queues it.
    pub fn begin(&self) -> Option<u64> {
        let generation = {
            let mut pending = self.lock();
            if !matches!(pending.mode, GamepadMode::Ready | GamepadMode::Rejected(_)) {
                return None;
            }
            let generation = pending.generation.checked_add(1)?;
            pending.generation = generation;
            pending.mode = GamepadMode::Pending(generation);
            pending
                .queue
                .push_back(GamepadRequest::Begin { generation });
            generation
        };
        (self.wake)();
        Some(generation)
    }
    /// Call only from live foreground controller sampling, not a keepalive task.
    pub fn sample(&self, generation: u64, state: GamepadState, now: Instant) -> bool {
        let accepted = {
            let mut pending = self.lock();
            if pending.mode != GamepadMode::Active(generation) {
                return false;
            }
            if pending.last_sample.is_some_and(|(old, sent)| {
                old == state && now.saturating_duration_since(sent) < RENEWAL
            }) {
                return true;
            }
            let request = GamepadRequest::State { generation, state };
            let replaced = if let Some(last) = pending.queue.back_mut()
                && request.supersedes(*last)
            {
                *last = request;
                true
            } else {
                false
            };
            if !replaced && pending.queue.len() >= CAPACITY {
                pending.stop();
                false
            } else {
                if !replaced {
                    pending.queue.push_back(request);
                }
                pending.last_sample = Some((state, now));
                true
            }
        };
        (self.wake)();
        accepted
    }
    pub fn stop(&self) {
        self.lock().stop();
        (self.wake)();
    }
    pub(crate) fn pop(&self) -> Option<GamepadRequest> {
        let mut pending = self.lock();
        pending
            .end
            .take()
            .map(|generation| GamepadRequest::End { generation })
            .or_else(|| pending.queue.pop_front())
    }
    pub(crate) fn observe(&self, status: GamepadStatus) {
        let mut pending = self.lock();
        match status {
            GamepadStatus::Available if pending.mode == GamepadMode::Unavailable => {
                pending.mode = GamepadMode::Ready
            }
            GamepadStatus::Capture { generation, state } if matches!(pending.mode, GamepadMode::Pending(id) | GamepadMode::Active(id) if id == generation) => {
                if state == GamepadCaptureState::Active {
                    pending.mode = GamepadMode::Active(generation);
                } else {
                    pending.stop();
                    pending.mode = GamepadMode::Rejected(state);
                }
            }
            _ => {}
        }
    }
    pub(crate) fn disconnect(&self) {
        let mut pending = self.lock();
        pending.stop();
        pending.end = None;
        pending.mode = GamepadMode::Disconnected;
    }
}
