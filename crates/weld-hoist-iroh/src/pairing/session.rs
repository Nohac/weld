//! Authorized catalog and window requests alongside the ordinary media streams.
use super::*;
use crate::{
    IrohDestinationPeer, IrohSourcePeer,
    admission::PendingConnection,
    channel::{LaneTask, PacketLane},
    framing::{read_record, write_record},
    host::HostLifetime,
    peer::{spawn_destination_peer, spawn_source_peer},
};
use iroh::Endpoint;
use std::{collections::VecDeque, sync::Weak};
use tokio::sync::mpsc;
use weld_access::{AccessAuthority, AccessSession};
use weld_hoist_core::session::services::{ServiceClient, ServiceSession};
use weld_hoist_encoded::{EncodedDestinationTransport, EncodedSourceTransport};
use weld_hoist_protocol::service::LANE_MARKER;
use weld_media::VideoCodec;

pub use weld_hoist_core::session::{DeviceAction, SessionId};
use weld_hoist_core::session::{Pending, SessionService, reply_channel};
use weld_hoist_protocol::device::{
    ApplicationInfo, DeviceHello as Hello, DeviceReply as Reply, DeviceRequest as Request,
    DeviceWelcome as Welcome,
};

/// Transport-owned peer handoff plus a handle to shared application services.
#[derive(Clone)]
pub struct DesktopSessions {
    access: AccessAuthority,
    peers: Arc<Mutex<VecDeque<(SessionId, IrohSourcePeer, AccessSession)>>>,
    services: SessionService,
    notifier: IrohNotifier,
    policy: IrohNotifier,
    codec: VideoCodec,
}
impl DesktopSessions {
    pub(super) fn close(&self) {
        if let Ok(mut peers) = self.peers.lock() {
            for (_, peer, access) in peers.drain(..) {
                self.services.leave(&access);
                peer.disconnect();
            }
        }
        let _ = self.policy.notify();
    }
    pub fn new(notifier: IrohNotifier, policy: IrohNotifier, codec: VideoCodec) -> Result<Self> {
        let wake = policy.clone();
        let services = SessionService::start(move || {
            let _ = wake.notify();
        })?;
        Ok(Self::with_service(notifier, policy, codec, services))
    }
    /// Attach this binding to the same service used by other transport listeners.
    pub fn with_service(
        notifier: IrohNotifier,
        policy: IrohNotifier,
        codec: VideoCodec,
        services: SessionService,
    ) -> Self {
        let wake = notifier.clone();
        Self {
            access: AccessAuthority::new(move || {
                let _ = wake.notify();
            }),
            peers: Arc::new(Mutex::new(VecDeque::new())),
            services,
            notifier,
            policy,
            codec,
        }
    }
    pub fn services(&self) -> &SessionService {
        &self.services
    }
    pub fn with_access(mut self, access: AccessAuthority) -> Self {
        self.access = access;
        self
    }
    pub fn take_peers(&self) -> Vec<(SessionId, IrohSourcePeer, AccessSession)> {
        self.peers
            .lock()
            .map(|mut peers| peers.drain(..).collect())
            .unwrap_or_default()
    }
    pub fn take_actions(&self) -> Vec<DeviceAction> {
        self.services.take_actions()
    }
    pub fn publish(&self, session: SessionId, applications: Vec<ApplicationInfo>) -> Result<()> {
        self.services.publish(session, applications)
    }
    async fn catalogue(&self, session: SessionId) -> Result<Vec<ApplicationInfo>> {
        self.services.catalogue(session).await
    }
    async fn join(
        &self,
        peer: IrohSourcePeer,
        access: AccessSession,
        permissions: DevicePermissions,
    ) -> Result<SessionId> {
        let id = self
            .services
            .join(access.clone(), permissions)?
            .wait()
            .await??;
        let admitted = (|| {
            let mut peers = self
                .peers
                .lock()
                .map_err(|_| anyhow::anyhow!("peer handoff unavailable"))?;
            ensure!(peers.len() < 8, "peer handoff full");
            peers.push_back((id, peer.clone(), access.clone()));
            Ok::<(), anyhow::Error>(())
        })();
        if let Err(error) = admitted {
            self.services.leave(&access);
            peer.disconnect();
            return Err(error);
        }
        self.policy.notify()?;
        Ok(id)
    }
    fn leave(&self, session: SessionId, access: &AccessSession) {
        self.services.leave(access);
        if let Ok(mut peers) = self.peers.lock() {
            peers.retain(|(id, _, _)| *id != session);
        }
        let _ = self.policy.notify();
    }
}

struct JoinedSession {
    desktop: DesktopSessions,
    id: SessionId,
    access: AccessSession,
}
impl Drop for JoinedSession {
    fn drop(&mut self) {
        self.desktop.leave(self.id, &self.access);
    }
}

pub(crate) async fn accept_session(
    host: Weak<HostLifetime>,
    authority: PairingHost,
    connection: Connection,
) -> Result<()> {
    let permissions = authority.authorize(&connection)?;
    ensure!(permissions.browse, "application browsing not permitted");
    let desktop = authority.desktop()?;
    let access = desktop
        .access
        .open(format!("device:{}", connection.remote_id()))?;
    let (mut send, mut recv) =
        tokio::time::timeout(Duration::from_secs(5), connection.accept_bi()).await??;
    let hello: Hello =
        tokio::time::timeout(Duration::from_secs(5), read_record(&mut recv)).await??;
    ensure!(
        hello.preferences.codecs.contains(&desktop.codec),
        "receiver does not support host codec"
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        write_record(
            &mut send,
            &Welcome {
                codec: desktop.codec,
            },
        ),
    )
    .await??;
    let (mut control_send, control_recv) =
        tokio::time::timeout(Duration::from_secs(5), connection.open_bi()).await??;
    tokio::time::timeout(Duration::from_secs(5), control_send.write_all(b"weldctl1")).await??;
    let media = tokio::time::timeout(Duration::from_secs(5), connection.open_uni()).await??;
    // Establish the service lane before the peer's diagnostic stream acceptor starts.
    let (mut service_send, service_recv) =
        tokio::time::timeout(Duration::from_secs(5), connection.open_bi()).await??;
    tokio::time::timeout(Duration::from_secs(5), service_send.write_all(LANE_MARKER)).await??;
    let peer = spawn_source_peer(
        host.upgrade().context("Iroh host closed")?,
        connection.clone(),
        control_send,
        control_recv,
        media,
        desktop.notifier.clone(),
        crate::presentation::SourcePresentation {
            codec: desktop.codec,
            stream_mode: hello.preferences.stream_mode,
        },
    );
    let recorder = peer.diagnostics();
    let id = desktop.join(peer, access.clone(), permissions).await?;
    let _joined = JoinedSession {
        desktop: desktop.clone(),
        id,
        access: access.clone(),
    };
    let control = async {
        let mut last_collection = None;
        let mut catalogue_updates: Option<weld_hoist_core::session::CatalogueSubscription> = None;
        loop {
            let reading = read_record::<_, Request>(&mut recv);
            tokio::pin!(reading);
            let request = loop {
                let changed = async {
                    match &catalogue_updates {
                        Some(updates) => updates.next().await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    request = &mut reading => break request?,
                    snapshot = changed => {
                        let snapshot = snapshot?;
                        ensure!(access.is_authorized(), "desktop access revoked");
                        session_io(&connection, Duration::from_secs(5),
                            weld_diagnostics::Operation::SessionWrite, recorder.as_ref(),
                            write_record(&mut send, &Reply::Catalogue(snapshot.applications.to_vec()))).await?;
                    }
                    reason = connection.closed() => return Err(reason.into()),
                }
            };
            let reply = match request {
                Request::Subscribe => {
                    catalogue_updates = Some(desktop.services.subscribe(id).await?);
                    Reply::Accepted(true)
                }
                Request::Diagnostics(report) => {
                    let allowed = authority.permits_diagnostics(connection.remote_id())
                        && last_collection
                            .is_none_or(|last: Instant| last.elapsed() >= Duration::from_secs(1));
                    last_collection = Some(Instant::now());
                    let response = if allowed {
                        host.upgrade().and_then(|host| {
                            host.reports.exchange(connection.remote_id(), *report).ok()
                        })
                    } else {
                        None
                    };
                    Reply::Diagnostics(response.map(Box::new))
                }
                Request::List => Reply::Catalogue(desktop.catalogue(id).await?),
                Request::Hoist(window) => {
                    ensure!(permissions.hoist, "hoisting not permitted");
                    let (answer, wait) = reply_channel();
                    desktop
                        .services
                        .action(DeviceAction::Hoist {
                            session: id,
                            window,
                            answer,
                        })
                        .await?;
                    Reply::Accepted(wait_for_action(&connection, wait).await?)
                }
                Request::Release => {
                    let (answer, wait) = reply_channel();
                    desktop
                        .services
                        .action(DeviceAction::Release {
                            session: id,
                            answer,
                        })
                        .await?;
                    Reply::Accepted(wait_for_action(&connection, wait).await?)
                }
            };
            ensure!(access.is_authorized(), "desktop access revoked");
            session_io(
                &connection,
                Duration::from_secs(5),
                weld_diagnostics::Operation::SessionWrite,
                recorder.as_ref(),
                write_record(&mut send, &reply),
            )
            .await?;
        }
    };
    let service_context = ServiceSession {
        service: desktop.services.clone(),
        id,
        access: access.clone(),
    };
    let service_authority = authority.clone();
    let peer_identity = connection.remote_id();
    let service_lane = service_context
        .serve(PacketLane::new(service_send, service_recv), move || {
            service_authority.permissions(peer_identity)
        });
    tokio::pin!(service_lane);
    tokio::pin!(control);
    tokio::select! {
        result = &mut control => result,
        result = &mut service_lane => {
            if let Err(error) = result { tracing::debug!(%error, "service lane ended"); }
            control.await
        },
    }
}

async fn wait_for_action(connection: &Connection, answer: Pending<bool>) -> Result<bool> {
    session_io(
        connection,
        Duration::from_secs(3),
        weld_diagnostics::Operation::Control,
        None,
        answer.wait(),
    )
    .await
}

/// Receiver media peer plus an independent, bounded application-control channel.
/// Dropping the session closes its peer and cancels any pending control exchange.
pub struct DeviceSession {
    pub services: ServiceClient,
    _service_task: LaneTask,
    diagnostic_reply: Arc<Mutex<Option<Option<weld_diagnostics::Report>>>>,
    pub peer: IrohDestinationPeer,
    catalogue: Arc<Mutex<Vec<ApplicationInfo>>>,
    commands: mpsc::Sender<Request>,
    error: Arc<Mutex<Option<String>>>,
}
impl Drop for DeviceSession {
    fn drop(&mut self) {
        self.peer.disconnect();
    }
}
impl DeviceSession {
    /// Explicit collection also shares this endpoint's sanitized session evidence.
    pub fn collect_diagnostics(&self, report: weld_diagnostics::Report) -> Result<()> {
        report.validate().map_err(anyhow::Error::msg)?;
        self.commands
            .try_send(Request::Diagnostics(Box::new(report)))
            .map_err(|_| anyhow::anyhow!("device diagnostic request queue unavailable"))
    }
    pub fn take_diagnostics(&self) -> Option<Option<weld_diagnostics::Report>> {
        self.diagnostic_reply.lock().ok()?.take()
    }
    pub fn take_error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|mut error| error.take())
    }
    pub fn applications(&self) -> Vec<ApplicationInfo> {
        self.catalogue.lock().map(|c| c.clone()).unwrap_or_default()
    }
    pub fn hoist(&self, window: u64) -> Result<()> {
        self.commands
            .try_send(Request::Hoist(window))
            .map_err(|_| anyhow::anyhow!("device command queue unavailable"))
    }
    pub fn release(&self) -> Result<()> {
        self.commands
            .try_send(Request::Release)
            .map_err(|_| anyhow::anyhow!("device command queue unavailable"))
    }
}
pub struct PendingDeviceSession {
    pub(crate) result: oneshot::Receiver<Result<DeviceSession, String>>,
    pub(crate) cancel: Option<oneshot::Sender<()>>,
}
impl PendingDeviceSession {
    pub fn poll(&mut self) -> Result<Option<DeviceSession>> {
        match self.result.try_recv() {
            Ok(result) => {
                self.cancel = None;
                result.map(Some).map_err(anyhow::Error::msg)
            }
            Err(oneshot::error::TryRecvError::Empty) => Ok(None),
            Err(_) => anyhow::bail!("device connection stopped"),
        }
    }
}
impl Drop for PendingDeviceSession {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

pub(crate) async fn connect_session(
    host: Weak<HostLifetime>,
    endpoint: Endpoint,
    profile: IrohConnectionProfile,
    preferences: crate::IrohReceiverPreferences,
    notifier: IrohNotifier,
    result: oneshot::Sender<Result<DeviceSession, String>>,
) -> Result<()> {
    let setup = async {
        let guard = PendingConnection::new(
            endpoint
                .connect(profile.endpoint_addr()?, SESSION_ALPN)
                .await?,
        );
        let (mut send, mut recv) = guard.connection.open_bi().await?;
        write_record(
            &mut send,
            &Hello {
                preferences: preferences.clone(),
            },
        )
        .await?;
        let welcome: Welcome = read_record(&mut recv).await?;
        ensure!(
            preferences.codecs.contains(&welcome.codec),
            "host selected an unsupported codec"
        );
        let (control_send, mut control_recv) = guard.connection.accept_bi().await?;
        let mut marker = [0; 8];
        control_recv.read_exact(&mut marker).await?;
        ensure!(&marker == b"weldctl1", "invalid device media channel");
        let media = guard.connection.accept_uni().await?;
        let (service_send, mut service_recv) = guard.connection.accept_bi().await?;
        service_recv.read_exact(&mut marker).await?;
        ensure!(&marker == LANE_MARKER, "invalid service channel");
        let service_wake = notifier.clone();
        let (services, driver) = ServiceClient::new(move || {
            let _ = service_wake.notify();
        });
        let service_connection = guard.connection.clone();
        let service_task = LaneTask(tokio::spawn(async move {
            tokio::select! {
                result = driver.run(PacketLane::new(service_send, service_recv)) => {
                    if let Err(error) = result { tracing::debug!(%error, "service lane ended"); }
                },
                _ = service_connection.closed() => {},
            }
        }));
        let peer = spawn_destination_peer(
            host.upgrade().context("Iroh host closed")?,
            guard.connection.clone(),
            control_send,
            control_recv,
            media,
            notifier.clone(),
            welcome.codec,
        );
        Ok::<_, anyhow::Error>((guard, send, recv, peer, services, service_task))
    };
    let (mut guard, mut send, mut recv, peer, services, service_task) =
        match tokio::time::timeout(Duration::from_secs(15), setup)
            .await
            .context("device connection timed out")
            .and_then(|result| result)
        {
            Ok(value) => value,
            Err(error) => {
                let _ = result.send(Err(format!("{error:#}")));
                notifier.notify()?;
                return Ok(());
            }
        };
    guard.admitted_session();
    let recorder = peer.diagnostics();
    let diagnostic_reply = Arc::new(Mutex::new(None));
    let catalogue = Arc::new(Mutex::new(Vec::new()));
    let error = Arc::new(Mutex::new(None));
    let (commands, mut requests) = mpsc::channel(16);
    if result
        .send(Ok(DeviceSession {
            services,
            _service_task: service_task,
            diagnostic_reply: diagnostic_reply.clone(),
            peer,
            catalogue: catalogue.clone(),
            commands,
            error: error.clone(),
        }))
        .is_err()
    {
        return Ok(());
    }
    notifier.notify()?;
    let receiving = async {
        loop {
            let reply: Reply = match read_record(&mut recv).await {
                Ok(reply) => reply,
                Err(error) => break Err::<(), _>(error),
            };
            match reply {
                Reply::Diagnostics(report) => {
                    if let Some(report) = &report {
                        report.validate().map_err(anyhow::Error::msg)?;
                    }
                    if let Ok(mut slot) = diagnostic_reply.lock() {
                        *slot = Some(report.map(|report| *report));
                    }
                }
                Reply::Catalogue(applications) => {
                    ensure!(applications.len() <= 256, "catalogue exceeds bound");
                    *catalogue
                        .lock()
                        .map_err(|_| anyhow::anyhow!("catalogue unavailable"))? = applications;
                }
                Reply::Accepted(false) => {
                    if let Ok(mut error) = error.lock() {
                        *error =
                            Some("The application is no longer available for this request".into());
                    }
                }
                Reply::Accepted(true) => {}
            }

            notifier.notify()?;
        }
    };
    let sending = async {
        session_io(
            &guard.connection,
            Duration::from_secs(5),
            weld_diagnostics::Operation::SessionWrite,
            recorder.as_ref(),
            write_record(&mut send, &Request::Subscribe),
        )
        .await?;
        while let Some(request) = requests.recv().await {
            session_io(
                &guard.connection,
                Duration::from_secs(5),
                weld_diagnostics::Operation::SessionWrite,
                recorder.as_ref(),
                write_record(&mut send, &request),
            )
            .await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::select! {
        result = receiving => result,
        result = sending => result,
        _ = guard.connection.closed() => Ok(()),
    }
}

/// Keep a live session's partially transferred record across slow exchanges.
/// The warning timer leaves the pinned operation intact; QUIC closure or an I/O
/// error ends the wait. Requests stay ordered and execute once after recovery.
async fn session_io<T>(
    connection: &Connection,
    warn_after: Duration,
    operation: weld_diagnostics::Operation,
    recorder: Option<&weld_diagnostics::Recorder>,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let started = tokio::time::Instant::now();
    tokio::pin!(future);
    let result = tokio::select! {
        result = &mut future => result,
        reason = connection.closed() => return Err(reason.into()),
        _ = tokio::time::sleep(warn_after) => {
            tracing::warn!(?operation, "device session control exchange delayed; retaining pending operation");
            let result = tokio::select! {
                result = &mut future => result,
                reason = connection.closed() => return Err(reason.into()),
            };
            if result.is_ok() {
                tracing::info!(?operation, elapsed_ms = started.elapsed().as_millis(), "device session control exchange resumed");
            }
            result
        }
    };
    if result.is_err()
        && connection.close_reason().is_none()
        && let Some(recorder) = recorder
    {
        recorder.record(weld_diagnostics::Observation::Failure {
            operation,
            cause: weld_diagnostics::Cause::ProtocolOrIo,
        });
    }
    result.with_context(|| format!("device session {operation:?} failed"))
}

#[cfg(test)]
mod tests;
