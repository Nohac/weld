//! Iroh stream capabilities consumed by the shared session supervisor.
use crate::{
    IrohDestinationPeer, IrohNotifier, IrohSourcePeer,
    admission::{Accepted, PendingConnection},
    channel::{PacketReader, PacketWriter},
    host::HostLifetime,
    pairing::{ConnectionLease, PairingHost, authenticated},
    peer::{SourceSetup, spawn_destination_peer, spawn_source_peer},
};
use anyhow::{Context, Result};
use std::sync::{Arc, Weak};
use weld_hoist_core::session::{
    client::{ControlConnection, DestinationChannels, DestinationConnection},
    pairing::{AuthenticatedPeer, PairingAuthority},
    server::{Connection, Services, SourceChannels, SourceContext, Wake},
};
use weld_hoist_protocol::{presentation::SourcePresentation, service::LANE_MARKER};
use weld_session::Connector;

pub struct IrohConnection {
    pub(crate) accepted: Accepted,
    pub(crate) host: Weak<HostLifetime>,
    pub(crate) io: tokio::runtime::Handle,
}
impl IrohConnection {
    fn attach_diagnostics(&self) {
        if let Some(host) = self.host.upgrade()
            && let Err(error) = host
                .reports
                .listen(&host.sessions, self.accepted.pending.connection.clone())
        {
            tracing::debug!(%error, "diagnostic service unavailable");
        }
    }
}

pub struct IrohConnector {
    pub(crate) endpoint: iroh::Endpoint,
    pub(crate) address: iroh::EndpointAddr,
    pub(crate) host: Weak<HostLifetime>,
    pub(crate) io: tokio::runtime::Handle,
    pub(crate) adb: Option<(Arc<crate::adb::Manager>, std::net::SocketAddr)>,
}
struct DialTask<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for DialTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl Connector for IrohConnector {
    type Connection = IrohConnection;
    async fn connect(self, protocol: &'static str) -> Result<IrohConnection> {
        let endpoint = self.endpoint;
        let mut task = DialTask(self.io.spawn(async move {
            let mut address = self.address;
            let lease = match self.adb {
                Some((manager, target)) => {
                    let lease = manager.connect(target).await?;
                    address = iroh::EndpointAddr::from_parts(
                        address.id,
                        [iroh::TransportAddr::Custom(lease.address())],
                    );
                    Some(lease)
                }
                None => None,
            };
            let pending =
                PendingConnection::new(endpoint.connect(address, protocol.as_bytes()).await?);
            if let Some(lease) = lease {
                lease.watch(&pending.connection)?;
            }
            Ok::<_, anyhow::Error>(pending)
        }));
        let pending = (&mut task.0).await??;
        Ok(IrohConnection {
            accepted: Accepted {
                pending,
                adb_route: None,
            },
            host: self.host,
            io: self.io,
        })
    }
}

/// Explicit-source attachment adds the transport's optional ADB route lifetime.
pub(crate) struct DirectConnection {
    pub connection: IrohConnection,
    pub links: Option<Arc<crate::adb::Manager>>,
}
impl weld_hoist_core::session::bootstrap::Connection for DirectConnection {
    type Writer = PacketWriter;
    type Reader = PacketReader;
    type Source = IrohSourcePeer;
    type Destination = IrohDestinationPeer;
    fn identity(&self) -> Result<AuthenticatedPeer> {
        self.connection
            .peer_identity()?
            .context("explicit admission requires peer identity evidence")
    }
    async fn open(&self) -> Result<(PacketWriter, PacketReader)> {
        self.connection.open_control().await
    }
    async fn accept(&self) -> Result<(PacketWriter, PacketReader)> {
        self.connection.accept_control().await
    }
    async fn source(
        mut self,
        lane: (PacketWriter, PacketReader),
        presentation: weld_hoist_core::session::bootstrap::Presentation,
        wake: weld_session::Wake,
    ) -> Result<IrohSourcePeer> {
        let weld_hoist_protocol::SurfaceMode::EncodedOpaque(codec) = presentation.mode else {
            anyhow::bail!("Iroh cannot transfer native buffer descriptors");
        };
        let connection = &self.connection.accepted.pending.connection;
        let media = connection.open_uni().await?;
        if let Some(links) = self.links {
            let _executor = self.connection.io.enter();
            links.admitted(
                self.connection
                    .accepted
                    .adb_route
                    .as_ref()
                    .context("ADB admission omitted route")?,
                connection,
            )?;
        }
        let peer = {
            let _executor = self.connection.io.enter();
            spawn_source_peer(
                self.connection.host.upgrade().context("Iroh host closed")?,
                connection.clone(),
                lane.0.into_stream()?,
                lane.1.into_stream()?,
                media,
                IrohNotifier::new(move || {
                    wake();
                    Ok(())
                }),
                SourceSetup {
                    presentation: SourcePresentation {
                        codec,
                        stream_mode: presentation.stream_mode,
                    },
                    diagnostics_access: crate::reports::Access::Participant,
                },
            )
        };
        self.connection.attach_diagnostics();
        self.connection.accepted.pending.hand_off();
        Ok(peer)
    }
    async fn destination(
        mut self,
        lane: (PacketWriter, PacketReader),
        presentation: weld_hoist_core::session::bootstrap::Presentation,
        wake: weld_session::Wake,
    ) -> Result<IrohDestinationPeer> {
        let weld_hoist_protocol::SurfaceMode::EncodedOpaque(codec) = presentation.mode else {
            anyhow::bail!("Iroh cannot transfer native buffer descriptors");
        };
        let connection = &self.connection.accepted.pending.connection;
        let media = connection.accept_uni().await?;
        let peer = {
            let _executor = self.connection.io.enter();
            spawn_destination_peer(
                self.connection.host.upgrade().context("Iroh host closed")?,
                connection.clone(),
                lane.0.into_stream()?,
                lane.1.into_stream()?,
                media,
                IrohNotifier::new(move || {
                    wake();
                    Ok(())
                }),
                codec,
            )
        };
        self.connection.attach_diagnostics();
        self.connection.accepted.pending.hand_off();
        Ok(peer)
    }
}
pub(crate) struct DirectDialer(pub IrohConnector);
impl Connector for DirectDialer {
    type Connection = DirectConnection;
    async fn connect(self, protocol: &'static str) -> Result<DirectConnection> {
        Ok(DirectConnection {
            connection: self.0.connect(protocol).await?,
            links: None,
        })
    }
}
pub(crate) struct DirectListener {
    pub incoming: crate::admission::IncomingQueue,
    pub links: Option<Arc<crate::adb::Manager>>,
}
impl weld_hoist_core::session::bootstrap::Incoming for DirectListener {
    type Connection = DirectConnection;
    async fn next(&mut self) -> Result<DirectConnection> {
        Ok(DirectConnection {
            connection: self
                .incoming
                .recv()
                .await
                .context("source listener closed")?,
            links: self.links.clone(),
        })
    }
}
impl ControlConnection for IrohConnection {
    type Writer = PacketWriter;
    type Reader = PacketReader;
    fn peer_identity(&self) -> Result<Option<weld_session::connection::PeerIdentity>> {
        authenticated(self.accepted.pending.connection.remote_id()).map(Some)
    }
    async fn open_control(&self) -> Result<(PacketWriter, PacketReader)> {
        let (send, receive) = self.accepted.pending.connection.open_bi().await?;
        Ok((PacketWriter::new(send), PacketReader::new(receive)))
    }
    async fn accept_control(&self) -> Result<(PacketWriter, PacketReader)> {
        let (send, receive) = self.accepted.pending.connection.accept_bi().await?;
        Ok((PacketWriter::new(send), PacketReader::new(receive)))
    }
    async fn closed(&self) -> Result<()> {
        Err(self.accepted.pending.connection.closed().await.into())
    }
}
impl DestinationConnection for IrohConnection {
    type Peer = IrohDestinationPeer;
    async fn destination(
        &self,
        codec: weld_media::VideoCodec,
        wake: weld_session::Wake,
    ) -> Result<DestinationChannels<IrohDestinationPeer, PacketWriter, PacketReader>> {
        let connection = &self.accepted.pending.connection;
        let (control_send, mut control_recv) = connection.accept_bi().await?;
        let mut marker = [0; 8];
        control_recv.read_exact(&mut marker).await?;
        anyhow::ensure!(&marker == b"weldctl1", "invalid media channel");
        let media = connection.accept_uni().await?;
        let (service_send, mut service_recv) = connection.accept_bi().await?;
        service_recv.read_exact(&mut marker).await?;
        anyhow::ensure!(&marker == LANE_MARKER, "invalid service channel");
        let peer = {
            let _executor = self.io.enter();
            spawn_destination_peer(
                self.host.upgrade().context("Iroh host closed")?,
                connection.clone(),
                control_send,
                control_recv,
                media,
                IrohNotifier::new(move || {
                    wake();
                    Ok(())
                }),
                codec,
            )
        };
        self.attach_diagnostics();
        let recorder = peer.diagnostics();
        Ok(DestinationChannels {
            peer,
            services: (
                PacketWriter::new(service_send),
                PacketReader::new(service_recv),
            ),
            recorder,
        })
    }
}
impl Connection for IrohConnection {
    type Peer = IrohSourcePeer;
    type Control = ConnectionLease;
    fn control(&self) -> ConnectionLease {
        ConnectionLease(self.accepted.pending.connection.weak_handle())
    }
    async fn open_source(
        &self,
        presentation: SourcePresentation,
        wake: Wake,
    ) -> Result<SourceChannels<IrohSourcePeer, PacketWriter, PacketReader>> {
        let connection = &self.accepted.pending.connection;
        let (mut control_send, control_recv) = connection.open_bi().await?;
        control_send.write_all(b"weldctl1").await?;
        let media = connection.open_uni().await?;
        // Open the service lane before the peer's diagnostic acceptor starts.
        let (mut service_send, service_recv) = connection.open_bi().await?;
        service_send.write_all(LANE_MARKER).await?;
        let host = self.host.upgrade().context("Iroh host closed")?;
        let diagnostics_access = crate::reports::Access::Paired(host.diagnostic_permission.clone());
        let peer = {
            let _executor = self.io.enter();
            spawn_source_peer(
                host,
                connection.clone(),
                control_send,
                control_recv,
                media,
                IrohNotifier::new(move || wake()),
                SourceSetup {
                    presentation,
                    diagnostics_access,
                },
            )
        };
        self.attach_diagnostics();
        let recorder = peer.diagnostics();
        Ok(SourceChannels {
            peer,
            services: (
                PacketWriter::new(service_send),
                PacketReader::new(service_recv),
            ),
            recorder,
        })
    }
}

/// Supplies this endpoint's shared authority and its existing report-archive adapter.
pub(crate) struct HostServices(pub PairingHost);
impl Services<IrohConnection> for HostServices {
    fn authority(&self) -> Result<PairingAuthority> {
        Ok(self.0.authority()?.clone())
    }
    fn source(&self, connection: &IrohConnection) -> Result<SourceContext<IrohSourcePeer>> {
        let host = connection.host.clone();
        let identity = connection.accepted.pending.connection.remote_id();
        Ok(SourceContext {
            sessions: self.0.listener()?,
            exchange: Arc::new(move |report| {
                host.upgrade()
                    .and_then(|host| host.reports.exchange(identity, report).ok())
            }),
        })
    }
}
