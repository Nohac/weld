//! Application-assembled services running over independently owned transports.
//!
//! A registry supplies protocol handlers and aggregate concurrency budgets.
//! The runtime owns listener and operation tasks through shutdown. Connection
//! types and service protocols remain owned by their implementing crates.

pub mod connection;
mod operation;
pub use operation::{Completion, Operation};

use anyhow::{Result, ensure};
use async_channel::{Receiver, Sender};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    thread,
};
use tokio::{sync::Semaphore, task::JoinSet};

pub type Task = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Protocol selected by the binding, with an owned connection. Dropping an
/// unhandled connection must retire its transport resources.
pub struct Accepted<C> {
    pub protocol: String,
    pub connection: C,
}

pub trait Listener: Send + 'static {
    type Connection: Send + 'static;
    fn accept(&mut self)
    -> impl Future<Output = Option<Result<Accepted<Self::Connection>>>> + Send;
}

/// A prepared transport dial target. Authentication and address resolution are
/// performed by the binding; service negotiation follows in the caller.
pub trait Connector: Send + 'static {
    type Connection: Send + 'static;
    fn connect(
        self,
        protocol: &'static str,
    ) -> impl Future<Output = Result<Self::Connection>> + Send;
}

/// Shared lifecycle control supplied by a transport or a service-owned peer.
pub trait ConnectionControl: Send + Sync {
    fn is_connected(&self) -> bool;
    fn close(&self);
}

struct Handler<C> {
    limit: usize,
    run: Arc<dyn Fn(C) -> Task + Send + Sync>,
}

/// Registrations are frozen when attached to a listener. Protocol identifiers
/// also supply the binding's advertised service list.
pub struct Registry<C>(BTreeMap<String, Handler<C>>);
impl<C> Default for Registry<C> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}
impl<C: Send + 'static> Registry<C> {
    pub fn register<F, Fut>(&mut self, protocol: &str, limit: usize, handler: F) -> Result<()>
    where
        F: Fn(C) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        ensure!(
            !protocol.is_empty() && protocol.len() <= 128,
            "invalid service identifier"
        );
        ensure!(
            limit > 0 && limit <= 256,
            "invalid service concurrency budget"
        );
        ensure!(
            !self.0.contains_key(protocol),
            "service already registered: {protocol}"
        );
        ensure!(self.0.len() < 64, "service registry limit reached");
        self.0.insert(
            protocol.into(),
            Handler {
                limit,
                run: Arc::new(move |connection| Box::pin(handler(connection))),
            },
        );
        Ok(())
    }
    pub fn protocols(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
    /// Routes accepted connections into a bounded handoff owned by a service.
    /// Shutdown drains unclaimed connections even when a caller retains a receiver.
    pub fn handoff(&mut self, protocol: &str, capacity: usize) -> Result<Receiver<C>> {
        ensure!(
            capacity > 0 && capacity <= 256,
            "invalid service handoff capacity"
        );
        let (sender, receiver) = async_channel::bounded(capacity);
        let queue = Arc::new(Queue(receiver.clone()));
        self.register(protocol, 256, move |connection| {
            let _queue = &queue;
            let result = sender
                .try_send(connection)
                .map_err(|_| anyhow::anyhow!("service handoff full or closed"));
            async move { result }
        })?;
        Ok(receiver)
    }
}
struct Queue<C>(Receiver<C>);
impl<C> Drop for Queue<C> {
    fn drop(&mut self) {
        self.0.close();
        while self.0.try_recv().is_ok() {}
    }
}

struct Job {
    protocol: String,
    limit: usize,
    task: Task,
}
enum Command {
    Listener(Task),
    Run(Job),
}
struct Owner {
    commands: Sender<Command>,
    listeners: Arc<Semaphore>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.commands.close();
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

/// One application-owned task supervisor for every registered transport/service.
#[derive(Clone)]
pub struct SessionRuntime(Arc<Owner>);
/// Submission handle for child services. It does not retain the supervisor's
/// owner; application shutdown closes it and drains its tasks.
#[derive(Clone)]
pub struct SessionTasks(Sender<Command>);
impl SessionTasks {
    pub fn spawn(
        &self,
        protocol: &str,
        limit: usize,
        task: impl Future<Output = Result<()>> + Send + 'static,
    ) -> Result<()> {
        ensure!(
            !protocol.is_empty() && protocol.len() <= 128 && limit > 0 && limit <= 256,
            "invalid service task budget"
        );
        self.0
            .try_send(Command::Run(Job {
                protocol: protocol.into(),
                limit,
                task: Box::pin(task),
            }))
            .map_err(|_| anyhow::anyhow!("session supervisor unavailable"))
    }
}
impl SessionRuntime {
    pub fn tasks(&self) -> SessionTasks {
        SessionTasks(self.0.commands.clone())
    }
    pub fn start() -> Result<Self> {
        let (commands, receiver) = async_channel::bounded(64);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let worker = thread::Builder::new().name("weld-session-io".into()).spawn(move || {
            runtime.block_on(async move {
                let mut listeners = JoinSet::new();
                let mut jobs = JoinSet::new();
                let mut active = BTreeMap::<String, (usize, Arc<Semaphore>)>::new();
                loop {
                    tokio::select! {
                        command = receiver.recv() => match command {
                            Ok(Command::Listener(task)) => { listeners.spawn(task); },
                            Ok(Command::Run(job)) => {
                                if active.len() >= 64 && !active.contains_key(&job.protocol) { continue; }
                                let entry = active.entry(job.protocol.clone()).or_insert_with(|| (job.limit, Arc::new(Semaphore::new(job.limit))));
                                // Every binding of one service uses the same aggregate budget.
                                if entry.0 == job.limit && let Ok(permit) = entry.1.clone().try_acquire_owned() {
                                    jobs.spawn(async move {
                                        let _permit = permit;
                                        (job.protocol, job.task.await)
                                    });
                                }
                            },
                            Err(_) => break,
                        },
                        result = jobs.join_next(), if !jobs.is_empty() => {
                            match result {
                                Some(Ok((protocol, Err(error)))) => tracing::debug!(%error, %protocol, "session service ended"),
                                Some(Err(error)) => tracing::warn!(%error, "session service task failed"),
                                _ => {},
                            }
                        },
                        _ = listeners.join_next(), if !listeners.is_empty() => {},
                    }
                }
                listeners.shutdown().await;
                jobs.shutdown().await;
                while receiver.try_recv().is_ok() {}
            });
        })?;
        Ok(Self(Arc::new(Owner {
            commands,
            listeners: Arc::new(Semaphore::new(16)),
            worker: Mutex::new(Some(worker)),
        })))
    }
    pub fn listen<L: Listener>(
        &self,
        mut listener: L,
        registry: Registry<L::Connection>,
    ) -> Result<()> {
        let permit = self
            .0
            .listeners
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("session listener limit reached"))?;
        let commands = self.0.commands.clone();
        self.0
            .commands
            .try_send(Command::Listener(Box::pin(async move {
                let _permit = permit;
                while let Some(accepted) = listener.accept().await {
                    let accepted = match accepted {
                        Ok(value) => value,
                        Err(error) => {
                            tracing::debug!(%error, "transport acceptance failed");
                            continue;
                        }
                    };
                    let Some(handler) = registry.0.get(&accepted.protocol) else {
                        continue;
                    };
                    let run = handler.run.clone();
                    let job = Job {
                        protocol: accepted.protocol,
                        limit: handler.limit,
                        task: Box::pin(async move { run(accepted.connection).await }),
                    };
                    if commands.send(Command::Run(job)).await.is_err() {
                        break;
                    }
                }
                Ok(())
            })))
            .map_err(|_| anyhow::anyhow!("session listener registration unavailable"))
    }
    /// Starts an operation whose result can be claimed once. Pending cancellation,
    /// unclaimed results and runtime shutdown all retire their owned resources.
    pub fn start_operation<T, F, Fut>(
        &self,
        protocol: &str,
        limit: usize,
        wake: Wake,
        retire: fn(&T),
        operation: F,
    ) -> Result<Operation<T>>
    where
        T: Send + 'static,
        F: FnOnce(Completion<T>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        ensure!(
            !protocol.is_empty() && protocol.len() <= 128 && limit > 0 && limit <= 256,
            "invalid service operation budget"
        );
        let (pending, completion, cancelled) = operation::channel(wake, retire);
        let failure = completion.clone();
        let task = Box::pin(async move {
            let future = operation(completion);
            let result = tokio::select! {
                biased;
                _ = async { if cancelled.recv().await.is_err() { std::future::pending::<()>().await; } } => Ok(()),
                result = future => result,
            };
            if let Err(error) = &result {
                failure.fail(format!("{error:#}"));
            }
            result
        });
        self.0
            .commands
            .try_send(Command::Run(Job {
                protocol: protocol.into(),
                limit,
                task,
            }))
            .map_err(|_| anyhow::anyhow!("session operation queue unavailable"))?;
        Ok(pending)
    }
}
