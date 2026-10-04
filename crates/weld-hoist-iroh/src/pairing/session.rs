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
use weld_client::ClientSurfaceId;
use weld_hoist_encoded::EncodedSourceTransport;
use weld_media::VideoCodec;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationInfo {
    pub window: u64,
    pub surface: ClientSurfaceId,
    pub title: String,
    pub app_id: String,
    pub available: bool,
    pub hoisted_here: bool,
}

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

struct DesktopState {
    peers: VecDeque<(SessionId, IrohSourcePeer)>,
    actions: VecDeque<DeviceAction>,
    catalogues: BTreeMap<SessionId, Vec<ApplicationInfo>>,
}
#[derive(Clone)]
pub struct DesktopSessions {
    state: Arc<Mutex<DesktopState>>,
    next: Arc<AtomicU64>,
    notifier: IrohNotifier,
    policy: IrohNotifier,
    codec: VideoCodec,
}
impl DesktopSessions {
    pub(super) fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            for (_, peer) in state.peers.drain(..) {
                peer.disconnect();
            }
            state.actions.clear();
            state.catalogues.clear();
        }
        let _ = self.policy.notify();
    }
    pub fn new(notifier: IrohNotifier, policy: IrohNotifier, codec: VideoCodec) -> Self {
        Self {
            state: Arc::new(Mutex::new(DesktopState {
                peers: VecDeque::new(),
                actions: VecDeque::new(),
                catalogues: BTreeMap::new(),
            })),
            next: Arc::new(AtomicU64::new(1)),
            notifier,
            policy,
            codec,
        }
    }
    pub fn take_peers(&self) -> Vec<(SessionId, IrohSourcePeer)> {
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
    pub fn publish(&self, session: SessionId, applications: Vec<ApplicationInfo>) {
        if let Ok(mut state) = self.state.lock()
            && let Some(catalogue) = state.catalogues.get_mut(&session)
        {
            *catalogue = applications.into_iter().take(256).collect();
        }
    }
    fn catalogue(&self, session: SessionId) -> Result<Vec<ApplicationInfo>> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop state unavailable"))?
            .catalogues
            .get(&session)
            .cloned()
            .context("desktop session ended")
    }
    fn join(&self, id: SessionId, peer: IrohSourcePeer) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop state unavailable"))?;
        ensure!(
            state.peers.len() < 8 && state.catalogues.len() < 8,
            "desktop session capacity reached"
        );
        state.peers.push_back((id, peer));
        state.catalogues.insert(id, Vec::new());
        drop(state);
        self.policy.notify()?;
        Ok(())
    }
    fn leave(&self, session: SessionId) {
        if let Ok(mut state) = self.state.lock() {
            state.catalogues.remove(&session);
            state.peers.retain(|(id, _)| *id != session);
            state.actions.retain(|action| match action {
                DeviceAction::Hoist { session: id, .. }
                | DeviceAction::Release { session: id, .. } => *id != session,
            });
        }
        let _ = self.policy.notify();
    }
    fn action(&self, action: DeviceAction) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop state unavailable"))?;
        ensure!(state.actions.len() < 64, "desktop command queue full");
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

#[derive(Serialize, Deserialize)]
enum Request {
    List,
    Hoist(u64),
    Release,
}
#[derive(Serialize, Deserialize)]
enum Reply {
    Catalogue(Vec<ApplicationInfo>),
    Accepted(bool),
}
#[derive(Serialize, Deserialize)]
struct Hello {
    codecs: Vec<VideoCodec>,
}
#[derive(Serialize, Deserialize)]
struct Welcome {
    codec: VideoCodec,
}

pub(crate) async fn accept_session(
    host: Weak<HostLifetime>,
    authority: PairingHost,
    connection: Connection,
) -> Result<()> {
    let permissions = authority.authorize(&connection)?;
    ensure!(permissions.browse, "application browsing not permitted");
    let desktop = authority.desktop()?;
    let (mut send, mut recv) =
        tokio::time::timeout(Duration::from_secs(5), connection.accept_bi()).await??;
    let hello: Hello =
        tokio::time::timeout(Duration::from_secs(5), read_record(&mut recv)).await??;
    ensure!(
        hello.codecs.contains(&desktop.codec),
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
    let peer = spawn_source_peer(
        host.upgrade().context("Iroh host closed")?,
        connection.clone(),
        control_send,
        control_recv,
        media,
        desktop.notifier.clone(),
        desktop.codec,
    );
    let id = SessionId(
        desktop
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
            .map_err(|_| anyhow::anyhow!("device session IDs exhausted"))?,
    );
    desktop.join(id, peer)?;
    let _joined = JoinedSession {
        desktop: desktop.clone(),
        id,
    };
    async {
        loop {
            let request: Request =
                tokio::time::timeout(Duration::from_secs(15), read_record(&mut recv)).await??;
            let reply = match request {
                Request::List => Reply::Catalogue(desktop.catalogue(id)?),
                Request::Hoist(window) => {
                    ensure!(permissions.hoist, "hoisting not permitted");
                    let (answer, wait) = oneshot::channel();
                    desktop.action(DeviceAction::Hoist {
                        session: id,
                        window,
                        answer,
                    })?;
                    Reply::Accepted(tokio::time::timeout(Duration::from_secs(3), wait).await??)
                }
                Request::Release => {
                    let (answer, wait) = oneshot::channel();
                    desktop.action(DeviceAction::Release {
                        session: id,
                        answer,
                    })?;
                    Reply::Accepted(tokio::time::timeout(Duration::from_secs(3), wait).await??)
                }
            };
            tokio::time::timeout(Duration::from_secs(5), write_record(&mut send, &reply)).await??;
        }
    }
    .await
}

/// Receiver media peer plus an independent, bounded application-control channel.
pub struct DeviceSession {
    pub peer: IrohDestinationPeer,
    catalogue: Arc<Mutex<Vec<ApplicationInfo>>>,
    commands: mpsc::Sender<Request>,
    error: Arc<Mutex<Option<String>>>,
}
impl DeviceSession {
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
    codecs: Vec<VideoCodec>,
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
                codecs: codecs.clone(),
            },
        )
        .await?;
        let welcome: Welcome = read_record(&mut recv).await?;
        ensure!(
            codecs.contains(&welcome.codec),
            "host selected an unsupported codec"
        );
        let (control_send, mut control_recv) = guard.connection.accept_bi().await?;
        let mut marker = [0; 8];
        control_recv.read_exact(&mut marker).await?;
        ensure!(&marker == b"weldctl1", "invalid device media channel");
        let media = guard.connection.accept_uni().await?;
        let peer = spawn_destination_peer(
            host.upgrade().context("Iroh host closed")?,
            guard.connection.clone(),
            control_send,
            control_recv,
            media,
            notifier.clone(),
            welcome.codec,
        );
        Ok::<_, anyhow::Error>((guard, send, recv, peer))
    };
    let (guard, mut send, mut recv, peer) =
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
    let catalogue = Arc::new(Mutex::new(Vec::new()));
    let error = Arc::new(Mutex::new(None));
    let (commands, mut requests) = mpsc::channel(16);
    if result
        .send(Ok(DeviceSession {
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
        tokio::time::timeout(Duration::from_secs(5), write_record(&mut send, &request)).await??;
        let reply = tokio::time::timeout(Duration::from_secs(5), read_record(&mut recv)).await??;
        match reply {
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
