//! Authorized catalog and window requests alongside the ordinary media streams.
use super::*;
use crate::{
    IrohDestinationPeer, IrohSourcePeer,
    admission::PendingConnection,
    framing::{read_record, write_record},
    host::HostLifetime,
    peer::{spawn_destination_peer, spawn_source_peer},
};
use iroh::Endpoint;
use std::{
    collections::VecDeque,
    sync::{
        Weak,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::mpsc;
use weld_access::{AccessAuthority, AccessSession};
use weld_hoist_encoded::{EncodedDestinationTransport, EncodedSourceTransport};
use weld_media::VideoCodec;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionId(pub u64);

use weld_hoist_protocol::device::{
    ApplicationInfo, DeviceHello as Hello, DeviceReply as Reply, DeviceRequest as Request,
    DeviceWelcome as Welcome,
};

pub enum DeviceAction {
    Hoist {
        session: SessionId,
        window: u64,
        answer: oneshot::Sender<bool>,
    },
    Release {
        session: SessionId,
        answer: oneshot::Sender<bool>,
    },
}

pub(super) struct DesktopState {
    peers: VecDeque<(SessionId, IrohSourcePeer, AccessSession)>,
    actions: VecDeque<DeviceAction>,
    pub(super) catalogues: BTreeMap<SessionId, (Vec<ApplicationInfo>, AccessSession)>,
    pub(super) thumbnails: thumbnail::Cache,
}
impl DesktopState {
    pub(super) fn prune_thumbnails(&mut self) {
        self.thumbnails.retain(|target| {
            self.catalogues.values().any(|(apps, access)| {
                access.is_authorized()
                    && apps
                        .iter()
                        .any(|app| app.window == target.window && app.surface == target.surface)
            })
        });
    }
}
#[derive(Clone)]
pub struct DesktopSessions {
    access: AccessAuthority,
    pub(super) state: Arc<Mutex<DesktopState>>,
    next: Arc<AtomicU64>,
    notifier: IrohNotifier,
    pub(super) policy: IrohNotifier,
    codec: VideoCodec,
}
impl DesktopSessions {
    pub(super) fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            for (_, peer, access) in state.peers.drain(..) {
                access.end();
                peer.disconnect();
            }
            state.actions.clear();
            for (_, access) in state.catalogues.values() {
                access.end();
            }
            state.catalogues.clear();
            state.prune_thumbnails();
        }
        let _ = self.policy.notify();
    }
    pub fn new(notifier: IrohNotifier, policy: IrohNotifier, codec: VideoCodec) -> Self {
        let wake = notifier.clone();
        Self {
            access: AccessAuthority::new(move || {
                let _ = wake.notify();
            }),
            state: Arc::new(Mutex::new(DesktopState {
                peers: VecDeque::new(),
                actions: VecDeque::new(),
                catalogues: BTreeMap::new(),
                thumbnails: thumbnail::Cache::default(),
            })),
            next: Arc::new(AtomicU64::new(1)),
            notifier,
            policy,
            codec,
        }
    }
    /// Shares the host's admission latch and administrative session inventory.
    pub fn with_access(mut self, access: AccessAuthority) -> Self {
        self.access = access;
        self
    }
    pub fn take_peers(&self) -> Vec<(SessionId, IrohSourcePeer, AccessSession)> {
        self.state
            .lock()
            .map(|mut state| state.peers.drain(..).collect())
            .unwrap_or_default()
    }
    pub fn take_actions(&self) -> Vec<DeviceAction> {
        self.state
            .lock()
            .map(|mut state| state.actions.drain(..).collect())
            .unwrap_or_default()
    }
    pub fn publish(&self, session: SessionId, mut applications: Vec<ApplicationInfo>) {
        if let Ok(mut state) = self.state.lock()
            && let Some(catalogue) = state.catalogues.get_mut(&session)
        {
            if !catalogue.1.is_authorized() {
                applications.clear();
            }
            applications.truncate(256);
            if catalogue.0 == applications {
                return;
            }
            catalogue.0 = applications;
            state.prune_thumbnails();
        }
    }
    fn catalogue(&self, session: SessionId) -> Result<Vec<ApplicationInfo>> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop state unavailable"))?
            .catalogues
            .get(&session)
            .filter(|(_, access)| access.is_authorized())
            .map(|(applications, _)| applications.clone())
            .context("desktop session ended")
    }
    fn join(&self, id: SessionId, peer: IrohSourcePeer, access: AccessSession) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop state unavailable"))?;
        ensure!(
            state.peers.len() < 8 && state.catalogues.len() < 8,
            "desktop session capacity reached"
        );
        ensure!(access.is_authorized(), "desktop access revoked");
        state.peers.push_back((id, peer, access.clone()));
        state.catalogues.insert(id, (Vec::new(), access));
        drop(state);
        self.policy.notify()?;
        Ok(())
    }
    fn leave(&self, session: SessionId) {
        if let Ok(mut state) = self.state.lock() {
            if let Some((_, access)) = state.catalogues.remove(&session) {
                access.end();
            }
            state.peers.retain(|(id, _, _)| *id != session);
            state.actions.retain(|action| match action {
                DeviceAction::Hoist { session: id, .. }
                | DeviceAction::Release { session: id, .. } => *id != session,
            });
            state.prune_thumbnails();
        }
        let _ = self.policy.notify();
    }
    fn action(&self, action: DeviceAction) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop state unavailable"))?;
        ensure!(state.actions.len() < 64, "desktop command queue full");
        let session = match &action {
            DeviceAction::Hoist { session, .. } | DeviceAction::Release { session, .. } => session,
        };
        ensure!(
            state
                .catalogues
                .get(session)
                .is_some_and(|(_, access)| access.is_authorized()),
            "desktop access revoked"
        );
        state.actions.push_back(action);
        drop(state);
        self.policy.notify()?;
        Ok(())
    }
}

struct JoinedSession {
    desktop: DesktopSessions,
    id: SessionId,
}
impl Drop for JoinedSession {
    fn drop(&mut self) {
        self.desktop.leave(self.id);
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
    // Establish the dedicated preview lane before the peer's diagnostic stream acceptor starts.
    let (mut thumbnail_send, thumbnail_recv) =
        tokio::time::timeout(Duration::from_secs(5), connection.open_bi()).await??;
    tokio::time::timeout(
        Duration::from_secs(5),
        thumbnail_send.write_all(thumbnail::MARKER),
    )
    .await??;
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
    let id = SessionId(
        desktop
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
            .map_err(|_| anyhow::anyhow!("device session IDs exhausted"))?,
    );
    let recorder = peer.diagnostics();
    desktop.join(id, peer, access.clone())?;
    let _joined = JoinedSession {
        desktop: desktop.clone(),
        id,
    };
    let control = async {
        let mut last_collection = None;
        loop {
            let request: Request = session_io(
                &connection,
                Duration::from_secs(15),
                weld_diagnostics::Operation::SessionRead,
                recorder.as_ref(),
                read_record(&mut recv),
            )
            .await?;
            let reply = match request {
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
                Request::List => Reply::Catalogue(desktop.catalogue(id)?),
                Request::Hoist(window) => {
                    ensure!(permissions.hoist, "hoisting not permitted");
                    let (answer, wait) = oneshot::channel();
                    desktop.action(DeviceAction::Hoist {
                        session: id,
                        window,
                        answer,
                    })?;
                    Reply::Accepted(wait_for_action(&connection, wait).await?)
                }
                Request::Release => {
                    let (answer, wait) = oneshot::channel();
                    desktop.action(DeviceAction::Release {
                        session: id,
                        answer,
                    })?;
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
    let previews = thumbnail::serve(
        desktop.clone(),
        id,
        authority.clone(),
        connection.clone(),
        thumbnail_send,
        thumbnail_recv,
    );
    tokio::pin!(previews);
    tokio::pin!(control);
    tokio::select! {
        result = &mut control => result,
        _ = &mut previews => control.await,
    }
}

async fn wait_for_action(connection: &Connection, answer: oneshot::Receiver<bool>) -> Result<bool> {
    session_io(
        connection,
        Duration::from_secs(3),
        weld_diagnostics::Operation::Control,
        None,
        async { answer.await.context("desktop action cancelled") },
    )
    .await
}

/// Receiver media peer plus an independent, bounded application-control channel.
/// Dropping the session closes its peer and cancels any pending control exchange.
pub struct DeviceSession {
    thumbnails: thumbnail::Client,
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
    /// Request a small preview without blocking catalogue updates or hoist commands.
    /// Retain revisions only for this connection's lifetime.
    pub fn thumbnail(
        &self,
        request: thumbnail::ThumbnailRequest,
    ) -> Result<thumbnail::PendingThumbnail> {
        self.thumbnails.request(request)
    }
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
        let (thumbnail_send, mut thumbnail_recv) = guard.connection.accept_bi().await?;
        thumbnail_recv.read_exact(&mut marker).await?;
        ensure!(&marker == thumbnail::MARKER, "invalid thumbnail channel");
        let thumbnails = thumbnail::Client::start(
            guard.connection.clone(),
            thumbnail_send,
            thumbnail_recv,
            notifier.clone(),
        );
        let peer = spawn_destination_peer(
            host.upgrade().context("Iroh host closed")?,
            guard.connection.clone(),
            control_send,
            control_recv,
            media,
            notifier.clone(),
            welcome.codec,
        );
        Ok::<_, anyhow::Error>((guard, send, recv, peer, thumbnails))
    };
    let (mut guard, mut send, mut recv, peer, thumbnails) =
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
            thumbnails,
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
    loop {
        let request = tokio::select! {
            _ = guard.connection.closed() => break,
            command = requests.recv() => match command { Some(command) => command, None => break },
            _ = tokio::time::sleep(Duration::from_millis(500)) => Request::List,
        };
        session_io(
            &guard.connection,
            Duration::from_secs(5),
            weld_diagnostics::Operation::SessionWrite,
            recorder.as_ref(),
            write_record(&mut send, &request),
        )
        .await?;
        let reply = session_io(
            &guard.connection,
            Duration::from_secs(5),
            weld_diagnostics::Operation::SessionRead,
            recorder.as_ref(),
            read_record(&mut recv),
        )
        .await?;
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
                    *error = Some("The application is no longer available for this request".into());
                }
            }
            Reply::Accepted(true) => {}
        }
        notifier.notify()?;
    }
    Ok(())
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
