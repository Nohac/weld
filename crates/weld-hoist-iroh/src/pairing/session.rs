//! Authorized catalog and window requests alongside the ordinary media streams.
use super::*;
use crate::{
    IrohDestinationPeer, IrohSourcePeer,
    admission::PendingConnection,
    channel::{LaneTask, PacketLane, PacketReader, PacketWriter},
    framing::{read_record, write_record},
    host::HostLifetime,
    peer::{spawn_destination_peer, spawn_source_peer},
};
use iroh::Endpoint;
use std::{collections::VecDeque, sync::Weak};
use weld_access::{AccessAuthority, AccessSession};
use weld_hoist_core::session::services::{ServiceClient, ServiceSession};
use weld_hoist_encoded::{EncodedDestinationTransport, EncodedSourceTransport};
use weld_hoist_protocol::service::LANE_MARKER;
use weld_media::VideoCodec;

pub use weld_hoist_core::session::{DeviceAction, SessionId};
use weld_hoist_core::session::{SessionService, device::DeviceClient};
use weld_hoist_protocol::device::{
    ApplicationInfo, DeviceHello as Hello, DeviceWelcome as Welcome,
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
        if let Err(error) = self.policy.notify() {
            self.leave(id, &access);
            peer.disconnect();
            return Err(error.into());
        }
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
    id: Option<SessionId>,
    access: AccessSession,
}
impl Drop for JoinedSession {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.desktop.leave(id, &self.access);
        } else {
            self.desktop.services.leave(&self.access);
        }
    }
}

pub(crate) async fn accept_session(
    host: Weak<HostLifetime>,
    authority: PairingHost,
    connection: Connection,
) -> Result<()> {
    let desktop = authority.desktop()?;
    let access = desktop
        .access
        .open(format!("device:{}", connection.remote_id()))?;
    let mut joined = JoinedSession {
        desktop: desktop.clone(),
        id: None,
        access: access.clone(),
    };
    let permissions = authority.admit(&connection, access.clone()).await?;
    ensure!(permissions.browse, "application browsing not permitted");
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
    joined.id = Some(id);
    let service_context = ServiceSession {
        service: desktop.services.clone(),
        id,
        access: access.clone(),
    };
    let service_authority = authority.clone();
    let peer_identity = connection.remote_id();
    let control_context = ServiceSession {
        service: desktop.services.clone(),
        id,
        access: access.clone(),
    };
    let control_authority = authority.clone();
    let control = control_context.serve_control(
        PacketWriter(send),
        PacketReader(recv),
        move || control_authority.permissions(peer_identity),
        move |report| {
            host.upgrade()
                .and_then(|host| host.reports.exchange(peer_identity, report).ok())
        },
        recorder.as_ref(),
    );
    let service_lane = service_context
        .serve(PacketLane::new(service_send, service_recv), move || {
            service_authority.permissions(peer_identity)
        });
    tokio::pin!(service_lane);
    tokio::pin!(control);
    tokio::select! {
        result = &mut control => result,
        reason = connection.closed() => Err(reason.into()),
        result = &mut service_lane => {
            if let Err(error) = result { tracing::debug!(%error, "service lane ended"); }
            tokio::select! {
                result = control => result,
                reason = connection.closed() => Err(reason.into()),
            }
        },
    }
}

/// Receiver media peer plus an independent, bounded application-control channel.
/// Dropping the session closes its peer and cancels any pending control exchange.
pub struct DeviceSession {
    pub services: ServiceClient,
    _service_task: LaneTask,
    pub control: DeviceClient,
    pub peer: IrohDestinationPeer,
}
impl Drop for DeviceSession {
    fn drop(&mut self) {
        self.peer.disconnect();
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
    let (mut guard, send, recv, peer, services, service_task) =
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
    let wake = notifier.clone();
    let (control, driver) = DeviceClient::new(move || {
        let _ = wake.notify();
    });
    if result
        .send(Ok(DeviceSession {
            services,
            _service_task: service_task,
            control,
            peer,
        }))
        .is_err()
    {
        return Ok(());
    }
    notifier.notify()?;
    tokio::select! {
        result = driver.run(PacketWriter(send), PacketReader(recv), recorder.as_ref()) => result,
        _ = guard.connection.closed() => Ok(()),
    }
}

#[cfg(test)]
mod tests;
