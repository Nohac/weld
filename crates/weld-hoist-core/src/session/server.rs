//! Shared listener supervision and per-client enrollment/session lifetimes.
//!
//! Bindings yield authenticated connection capabilities. This owner selects
//! handlers, bounds concurrent sessions, and cancels every child on shutdown.

#[cfg(test)]
mod tests;

use super::{
    SessionId, SessionService,
    admission::SourceAdmissions,
    channel::{PacketReceive, PacketSend, receive_record, send_record},
    negotiation,
    pairing::{AuthenticatedPeer, ConnectionControl, PairingAuthority, enrollment},
    services::ServiceSession,
};
use anyhow::{Context, Result, ensure};
use async_channel::{Receiver, Sender};
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tokio::{task::JoinSet, time::timeout};
use weld_access::{AccessAuthority, AccessSession};
use weld_diagnostics::{Recorder, Report};
use weld_hoist_protocol::{
    device::{DeviceHello, DevicePermissions, DeviceWelcome},
    presentation::SourcePresentation,
};
use weld_media::VideoCodec;

pub type Wake = Arc<dyn Fn() -> io::Result<()> + Send + Sync>;

/// Application protocol selected during transport setup.
#[derive(Clone, Copy, Debug)]
pub enum Protocol {
    Enrollment,
    Device,
    DirectSource,
}

pub struct Accepted<C> {
    pub protocol: Protocol,
    pub connection: C,
}

/// Transport acceptance may own concurrent cryptographic handshake work.
pub trait Listener: Send + 'static {
    type Connection: Connection;
    fn accept(&mut self)
    -> impl Future<Output = Option<Result<Accepted<Self::Connection>>>> + Send;
}

/// Authenticated connection capabilities consumed by the session owner.
/// Dropping the connection closes it, including incomplete setup attempts.
pub trait Connection: Send + Sync + 'static {
    type Writer: PacketSend + 'static;
    type Reader: PacketReceive + 'static;
    type Peer: ConnectionControl + Clone + Send + Sync + 'static;
    type Control: ConnectionControl + 'static;
    fn identity(&self) -> Result<AuthenticatedPeer>;
    fn control(&self) -> Self::Control;
    fn closed(&self) -> impl Future<Output = Result<()>> + Send;
    fn accept_control(&self) -> impl Future<Output = Result<(Self::Writer, Self::Reader)>> + Send;
    fn open_source(
        &self,
        presentation: SourcePresentation,
        wake: Wake,
    ) -> impl Future<Output = Result<SourceChannels<Self::Peer, Self::Writer, Self::Reader>>> + Send;
}

/// Media transport and independent service lane prepared by the binding.
pub struct SourceChannels<P, W, R> {
    pub peer: P,
    pub services: (W, R),
    pub recorder: Option<Recorder>,
}

/// Host assembly supplies shared policy owners and session-scoped report access.
pub trait Services<C: Connection>: Send + Sync + 'static {
    fn authority(&self) -> Result<PairingAuthority>;
    fn source(&self, connection: &C) -> Result<SourceContext<C::Peer>>;
}
pub struct SourceContext<P: ConnectionControl + Clone> {
    pub sessions: SourceSessions<P>,
    pub exchange: Arc<dyn Fn(Report) -> Option<Report> + Send + Sync>,
}

/// Host-side relay handoff and shared services attached to admitted source clients.
#[derive(Clone)]
pub struct SourceSessions<P: ConnectionControl + Clone> {
    access: AccessAuthority,
    peers: SourceAdmissions<P>,
    services: SessionService,
    notify: Wake,
    policy: Wake,
    codec: VideoCodec,
}
impl<P: ConnectionControl + Clone> SourceSessions<P> {
    pub fn new(
        notify: impl Fn() -> io::Result<()> + Send + Sync + 'static,
        policy: impl Fn() -> io::Result<()> + Send + Sync + 'static,
        codec: VideoCodec,
    ) -> Result<Self> {
        let policy: Wake = Arc::new(policy);
        let wake = policy.clone();
        let services = SessionService::start(move || {
            let _ = wake();
        })?;
        Ok(Self::with_service(
            Arc::new(notify),
            policy,
            codec,
            services,
        ))
    }
    pub fn with_service(
        notify: Wake,
        policy: Wake,
        codec: VideoCodec,
        services: SessionService,
    ) -> Self {
        let wake = notify.clone();
        Self {
            access: AccessAuthority::new(move || {
                let _ = wake();
            }),
            peers: SourceAdmissions::default(),
            services,
            notify,
            policy,
            codec,
        }
    }
    pub fn with_access(mut self, access: AccessAuthority) -> Self {
        self.access = access;
        self
    }
    pub fn services(&self) -> &SessionService {
        &self.services
    }
    pub fn admissions(&self) -> SourceAdmissions<P> {
        self.peers.clone()
    }
    pub fn close(&self) {
        self.peers.close();
        let _ = (self.policy)();
    }
    async fn join(
        &self,
        peer: P,
        access: AccessSession,
        permissions: DevicePermissions,
    ) -> Result<SessionId> {
        let id = self
            .services
            .join(access.clone(), permissions)?
            .wait()
            .await??;
        if let Err(error) = self
            .peers
            .admit(id, peer.clone(), access.clone(), self.codec)
        {
            self.services.leave(&access);
            peer.close();
            return Err(error);
        }
        if let Err(error) = (self.policy)() {
            self.leave(id, &access);
            peer.close();
            return Err(error.into());
        }
        Ok(id)
    }
    fn leave(&self, id: SessionId, access: &AccessSession) {
        self.services.leave(access);
        self.peers.remove(id);
        let _ = (self.policy)();
    }
}
struct JoinedSession<P: ConnectionControl + Clone> {
    source: SourceSessions<P>,
    id: Option<SessionId>,
    access: AccessSession,
}
impl<P: ConnectionControl + Clone> Drop for JoinedSession<P> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.source.leave(id, &self.access);
        } else {
            self.source.services.leave(&self.access);
        }
    }
}

type ClientTask = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
type ListenerTask = Pin<Box<dyn Future<Output = ()> + Send>>;
struct Incoming {
    protocol: Protocol,
    task: ClientTask,
}
struct DirectQueue<C>(Receiver<C>);
impl<C> Drop for DirectQueue<C> {
    fn drop(&mut self) {
        self.0.close();
        while self.0.try_recv().is_ok() {}
    }
}
struct RuntimeOwner {
    listeners: Sender<ListenerTask>,
    incoming: Sender<Incoming>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}
impl Drop for RuntimeOwner {
    fn drop(&mut self) {
        self.listeners.close();
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

/// One host-owned supervisor for incoming clients from every registered transport.
/// All listeners feed the same bounded per-protocol task sets.
#[derive(Clone)]
pub struct SessionRuntime(Arc<RuntimeOwner>);
impl SessionRuntime {
    pub fn start() -> Result<Self> {
        let (listeners, pending_listeners) = async_channel::bounded::<ListenerTask>(8);
        let (incoming, arrivals) = async_channel::bounded::<Incoming>(16);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let worker = thread::Builder::new().name("weld-session-io".into()).spawn(move || {
            runtime.block_on(async move {
                let mut listener_tasks = JoinSet::new();
                let mut enrollment = JoinSet::new();
                let mut sessions = JoinSet::new();
                loop {
                    tokio::select! {
                        registration = pending_listeners.recv() => match registration {
                            Ok(task) => { listener_tasks.spawn(task); },
                            Err(_) => break,
                        },
                        incoming = arrivals.recv() => {
                            let Ok(incoming) = incoming else { break; };
                            match incoming.protocol {
                                Protocol::Enrollment if enrollment.len() < 8 => { enrollment.spawn(incoming.task); },
                                Protocol::Device if sessions.len() < 8 => { sessions.spawn(incoming.task); },
                                _ => {},
                            }
                        },
                        result = enrollment.join_next(), if !enrollment.is_empty() => { completed(result); },
                        result = sessions.join_next(), if !sessions.is_empty() => { completed(result); },
                        _ = listener_tasks.join_next(), if !listener_tasks.is_empty() => {},
                    }
                }
                listener_tasks.shutdown().await;
                enrollment.shutdown().await;
                sessions.shutdown().await;
            });
        })?;
        Ok(Self(Arc::new(RuntimeOwner {
            listeners,
            incoming,
            worker: Mutex::new(Some(worker)),
        })))
    }
    /// Registers an incoming connection source. Explicitly approved development
    /// connections are returned to their pending source-admission operation.
    pub fn listen<L: Listener>(
        &self,
        mut listener: L,
        services: impl Services<L::Connection>,
    ) -> Result<Receiver<L::Connection>> {
        let (sender, direct) = async_channel::bounded(8);
        let queue = DirectQueue(direct.clone());
        let services = Arc::new(services);
        let incoming = self.0.incoming.clone();
        self.0
            .listeners
            .try_send(Box::pin(async move {
                let _queue = queue;
                while let Some(accepted) = listener.accept().await {
                    let accepted = match accepted {
                        Ok(value) => value,
                        Err(error) => {
                            tracing::debug!(%error, "transport acceptance failed");
                            continue;
                        }
                    };
                    if matches!(accepted.protocol, Protocol::DirectSource) {
                        let _ = sender.try_send(accepted.connection);
                        continue;
                    }
                    let services = services.clone();
                    let protocol = accepted.protocol;
                    let task = Box::pin(async move {
                        match protocol {
                            Protocol::Enrollment => {
                                serve_enrollment(accepted.connection, services.as_ref()).await
                            }
                            Protocol::Device => {
                                serve_device(accepted.connection, services.as_ref()).await
                            }
                            Protocol::DirectSource => Ok(()),
                        }
                    });
                    if incoming.send(Incoming { protocol, task }).await.is_err() {
                        break;
                    }
                }
            }))
            .map_err(|_| anyhow::anyhow!("session listener registration unavailable"))?;
        Ok(direct)
    }
}
fn completed(result: Option<std::result::Result<Result<()>, tokio::task::JoinError>>) {
    match result {
        Some(Ok(Err(error))) => tracing::debug!(%error, "client session ended"),
        Some(Err(error)) => tracing::warn!(%error, "client session task failed"),
        _ => {}
    }
}
async fn setup<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    timeout(Duration::from_secs(5), future)
        .await
        .context("session setup timed out")?
}
struct Duplex<W, R>(W, R);
impl<W: PacketSend, R: Send> PacketSend for Duplex<W, R> {
    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.0.send(bytes).await
    }
    async fn finish(&mut self) -> Result<()> {
        self.0.finish().await
    }
}
impl<W: Send, R: PacketReceive> PacketReceive for Duplex<W, R> {
    async fn receive(&mut self, maximum: usize) -> Result<Vec<u8>> {
        self.1.receive(maximum).await
    }
}
async fn serve_enrollment<C: Connection>(connection: C, services: &impl Services<C>) -> Result<()> {
    let authority = services.authority()?;
    let identity = connection.identity()?;
    let exchange = async {
        let (send, receive) = setup(connection.accept_control()).await?;
        let mut lane = Duplex(send, receive);
        enrollment::serve(&authority, identity, &mut lane).await?;
        setup(lane.finish()).await
    };
    tokio::select! { result = exchange => result, result = connection.closed() => result }
}
async fn serve_device<C: Connection>(connection: C, provider: &impl Services<C>) -> Result<()> {
    let authority = provider.authority()?;
    let identity = connection.identity()?;
    let context = provider.source(&connection)?;
    let source = context.sessions;
    let access = source.access.open(identity.label())?;
    let mut joined = JoinedSession {
        source: source.clone(),
        id: None,
        access: access.clone(),
    };
    let session = async {
        let permissions = authority
            .admit(identity.clone(), access.clone(), connection.control())?
            .wait()
            .await??;
        ensure!(permissions.browse, "application browsing not permitted");
        let (mut send, mut receive) = setup(connection.accept_control()).await?;
        let hello: DeviceHello = setup(receive_record(&mut receive)).await?;
        let presentation = negotiation::presentation(&hello.preferences, source.codec)?;
        setup(send_record(
            &mut send,
            &DeviceWelcome {
                codec: source.codec,
            },
        ))
        .await?;
        let SourceChannels {
            peer,
            services: (service_send, service_recv),
            recorder,
        } = setup(connection.open_source(presentation, source.notify.clone())).await?;
        joined.id = Some(source.join(peer, access.clone(), permissions).await?);
        let id = joined
            .id
            .context("source admission omitted session identity")?;
        let control_context = ServiceSession {
            service: source.services.clone(),
            id,
            access: access.clone(),
        };
        let service_context = ServiceSession {
            service: source.services.clone(),
            id,
            access: access.clone(),
        };
        let control = control_context.serve_control(
            send,
            receive,
            || authority.permissions(&identity),
            |report| (context.exchange)(report),
            recorder.as_ref(),
        );
        let service_lane = service_context.serve(Duplex(service_send, service_recv), || {
            authority.permissions(&identity)
        });
        tokio::pin!(control);
        tokio::select! {
            result = &mut control => result,
            result = service_lane => {
                if let Err(error) = result { tracing::debug!(%error, "service lane ended"); }
                control.await
            }
        }
    };
    tokio::select! { result = session => result, result = connection.closed() => result }
}
