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
use std::sync::Weak;
use weld_access::{AccessAuthority, AccessSession};
use weld_hoist_core::session::admission::SourceAdmissions;
use weld_hoist_core::session::negotiation;
use weld_hoist_core::session::services::{ServiceClient, ServiceSession};
use weld_hoist_encoded::{EncodedDestinationTransport, EncodedSourceTransport};
use weld_hoist_protocol::service::LANE_MARKER;
use weld_media::VideoCodec;

use weld_hoist_core::session::SessionId;
use weld_hoist_core::session::{SessionService, device::DeviceClient};
use weld_hoist_protocol::device::{DeviceHello as Hello, DeviceWelcome as Welcome};

/// Transport-owned peer handoff plus a handle to shared application services.
#[derive(Clone)]
pub struct SessionListener {
    access: AccessAuthority,
    peers: SourceAdmissions<IrohSourcePeer>,
    services: SessionService,
    notifier: IrohNotifier,
    policy: IrohNotifier,
    codec: VideoCodec,
}
impl SessionListener {
    pub(super) fn close(&self) {
        self.peers.close();
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
            peers: SourceAdmissions::default(),
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
    pub fn admissions(&self) -> SourceAdmissions<IrohSourcePeer> {
        self.peers.clone()
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
        let admitted = self
            .peers
            .admit(id, peer.clone(), access.clone(), peer.codec());
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
        self.peers.remove(session);
        let _ = self.policy.notify();
    }
}

struct JoinedSession {
    listener: SessionListener,
    id: Option<SessionId>,
    access: AccessSession,
}
impl Drop for JoinedSession {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.listener.leave(id, &self.access);
        } else {
            self.listener.services.leave(&self.access);
        }
    }
}

pub(crate) async fn accept_session(
    host: Weak<HostLifetime>,
    authority: PairingHost,
    connection: Connection,
) -> Result<()> {
    let listener = authority.listener()?;
    let access = listener
        .access
        .open(format!("device:{}", connection.remote_id()))?;
    let mut joined = JoinedSession {
        listener: listener.clone(),
        id: None,
        access: access.clone(),
    };
    let permissions = authority.admit(&connection, access.clone()).await?;
    ensure!(permissions.browse, "application browsing not permitted");
    let (mut send, mut recv) =
        tokio::time::timeout(Duration::from_secs(5), connection.accept_bi()).await??;
    let hello: Hello =
        tokio::time::timeout(Duration::from_secs(5), read_record(&mut recv)).await??;
    let presentation = negotiation::presentation(&hello.preferences, listener.codec)?;
    tokio::time::timeout(
        Duration::from_secs(5),
        write_record(
            &mut send,
            &Welcome {
                codec: listener.codec,
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
        listener.notifier.clone(),
        presentation,
    );
    let recorder = peer.diagnostics();
    let id = listener.join(peer, access.clone(), permissions).await?;
    joined.id = Some(id);
    let service_context = ServiceSession {
        service: listener.services.clone(),
        id,
        access: access.clone(),
    };
    let service_authority = authority.clone();
    let peer_identity = connection.remote_id();
    let control_context = ServiceSession {
        service: listener.services.clone(),
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
    preferences: weld_hoist_protocol::presentation::ReceiverPreferences,
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
        negotiation::presentation(&preferences, welcome.codec)?;
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
