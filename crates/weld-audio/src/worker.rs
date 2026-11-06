//! PipeWire objects remain on their owning event-loop thread.

use anyhow::{Context, Result};
use pipewire::{context::ContextRc, core::CoreRc, main_loop::MainLoopRc};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Clone, Default)]
pub(crate) struct Failure(Arc<Mutex<Option<String>>>);
impl Failure {
    pub fn set(&self, error: impl std::fmt::Display) {
        if let Ok(mut failure) = self.0.lock() {
            *failure = Some(error.to_string());
        }
    }
    pub fn check(&self) -> Result<()> {
        let failure = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("audio worker state unavailable"))?;
        if let Some(error) = &*failure {
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

enum Command {
    Wake,
    Stop,
}
type WakeHandler = Box<dyn Fn()>;

pub(crate) struct Worker {
    stop: pipewire::channel::Sender<Command>,
    wake_pending: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    ready: Mutex<mpsc::Receiver<Result<(), String>>>,
    pub failure: Failure,
}
impl Worker {
    pub fn spawn<R: 'static>(
        setup: impl FnOnce(&CoreRc, Failure) -> Result<R> + Send + 'static,
    ) -> Result<Self> {
        Self::with_wake(move |core, failure| Ok((setup(core, failure)?, None)))
    }

    pub fn with_wake<R: 'static>(
        setup: impl FnOnce(&CoreRc, Failure) -> Result<(R, Option<WakeHandler>)> + Send + 'static,
    ) -> Result<Self> {
        let (stop, receiver) = pipewire::channel::channel();
        let wake_pending = Arc::new(AtomicBool::new(false));
        let pending = wake_pending.clone();
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let failure = Failure::default();
        let worker_failure = failure.clone();
        let thread = thread::Builder::new()
            .name("weld-audio".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    pipewire::init();
                    let mainloop = MainLoopRc::new(None)?;
                    let context = ContextRc::new(&mainloop, None)?;
                    let core = context.connect_rc(None).context("connecting to PipeWire")?;
                    let failed = worker_failure.clone();
                    let quit = mainloop.clone();
                    let _errors = core
                        .add_listener_local()
                        .error(move |_, _, _, message| {
                            failed.set(message);
                            quit.quit();
                        })
                        .register();
                    let (_resources, wake) = setup(&core, worker_failure.clone())?;
                    let quit = mainloop.clone();
                    let _stop = receiver.attach(mainloop.loop_(), move |command| match command {
                        Command::Stop => quit.quit(),
                        Command::Wake => {
                            pending.store(false, Ordering::Release);
                            if let Some(wake) = &wake {
                                wake();
                            }
                        }
                    });
                    let sequence = core.sync(0)?;
                    let announce = ready_tx.clone();
                    let _ready = core
                        .add_listener_local()
                        .done(move |id, completed| {
                            if id == pipewire::core::PW_ID_CORE && completed == sequence {
                                let _ = announce.try_send(Ok(()));
                            }
                        })
                        .register();
                    mainloop.run();
                    worker_failure.check()
                })();
                if let Err(error) = result {
                    let message = format!("{error:#}");
                    worker_failure.set(&message);
                    let _ = ready_tx.try_send(Err(message));
                }
            })?;
        Ok(Self {
            stop,
            wake_pending,
            thread: Some(thread),
            ready: Mutex::new(ready),
            failure,
        })
    }

    pub fn wake(&self) {
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            let _ = self.stop.send(Command::Wake);
        }
    }

    /// Only launch/control workers may wait; compositor servicing stays asynchronous.
    pub fn wait_ready(&self) -> Result<()> {
        let result = self
            .ready
            .lock()
            .map_err(|_| anyhow::anyhow!("audio readiness unavailable"))?
            .recv_timeout(Duration::from_secs(3));
        self.failure.check()?;
        result
            .context("PipeWire audio setup reply unavailable")?
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
