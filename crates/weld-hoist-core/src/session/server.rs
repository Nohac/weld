//! Shared listener supervision and per-client enrollment/session lifetimes.
//!
//! Bindings yield authenticated connection capabilities. This owner selects
//! handlers, bounds concurrent sessions, and cancels every child on shutdown.

#[cfg(test)]
mod tests;

use super::{
    SessionId, SessionService,
    admission::SourceAdmissions,
    channel::{PacketSend, receive_record, send_record},
    negotiation,
    pairing::{ConnectionControl, PairingAuthority, enrollment},
    services::ServiceSession,
};
use anyhow::{Context, Result, ensure};
use async_channel::Receiver;
use std::{future::Future, io, sync::Arc, time::Duration};
use tokio::time::timeout;
use weld_access::{AccessAuthority, AccessSession};
use weld_diagnostics::{Recorder, Report};
use weld_hoist_protocol::{
    device::{DeviceHello, DevicePermissions, DeviceWelcome},
    presentation::SourcePresentation,
};
use weld_media::VideoCodec;
pub use weld_session::{Accepted, Listener, SessionRuntime};
use weld_session::{Registry, connection::Duplex};

pub type Wake = Arc<dyn Fn() -> io::Result<()> + Send + Sync>;

pub const ENROLLMENT_SERVICE: &str = "weld/pair/1";
pub const DEVICE_SERVICE: &str = "weld/device/1";
pub const DIRECT_SERVICE: &str = "weld/hoist/1";

/// Authenticated connection capabilities consumed by the session owner.
/// Dropping the connection closes it, including incomplete setup attempts.
pub trait Connection: weld_session::connection::PacketConnection {
    type Peer: ConnectionControl + Clone + Send + Sync + 'static;
    type Control: ConnectionControl + 'static;
    fn control(&self) -> Self::Control;
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

/// Assemble hoist handlers into an extensible registry. Applications may register
/// additional services before attaching the registry to a transport listener.
pub fn register<C: Connection>(
    registry: &mut Registry<C>,
    services: impl Services<C>,
) -> Result<Receiver<C>> {
    let services = Arc::new(services);
    let enrollment = services.clone();
    registry.register(ENROLLMENT_SERVICE, 8, move |connection| {
        let provider = enrollment.clone();
        async move { serve_enrollment(connection, provider.as_ref()).await }
    })?;
    registry.register(DEVICE_SERVICE, 8, move |connection| {
        let provider = services.clone();
        async move { serve_device(connection, provider.as_ref()).await }
    })?;
    registry.handoff(DIRECT_SERVICE, 8)
}
async fn setup<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    timeout(Duration::from_secs(5), future)
        .await
        .context("session setup timed out")?
}
async fn serve_enrollment<C: Connection>(connection: C, services: &impl Services<C>) -> Result<()> {
    let authority = services.authority()?;
    let identity = connection
        .peer_identity()?
        .context("enrollment requires peer identity evidence")?;
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
    let identity = connection
        .peer_identity()?
        .context("paired session requires peer identity evidence")?;
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
                instance: source.services.instance(),
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
