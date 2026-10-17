//! Iroh stream capabilities consumed by the shared session supervisor.
use crate::{
    IrohNotifier, IrohSourcePeer,
    admission::Accepted,
    channel::{PacketReader, PacketWriter},
    host::HostLifetime,
    pairing::{ConnectionLease, PairingHost, authenticated},
    peer::spawn_source_peer,
};
use anyhow::{Context, Result};
use std::sync::{Arc, Weak};
use weld_hoist_core::session::{
    pairing::{AuthenticatedPeer, PairingAuthority},
    server::{Connection, Services, SourceChannels, SourceContext, Wake},
};
use weld_hoist_protocol::{presentation::SourcePresentation, service::LANE_MARKER};

pub(crate) struct IrohConnection {
    pub accepted: Accepted,
    pub host: Weak<HostLifetime>,
    pub io: tokio::runtime::Handle,
}
impl Connection for IrohConnection {
    type Writer = PacketWriter;
    type Reader = PacketReader;
    type Peer = IrohSourcePeer;
    type Control = ConnectionLease;
    fn identity(&self) -> Result<AuthenticatedPeer> {
        authenticated(self.accepted.pending.connection.remote_id())
    }
    fn control(&self) -> ConnectionLease {
        ConnectionLease(self.accepted.pending.connection.weak_handle())
    }
    async fn closed(&self) -> Result<()> {
        Err(self.accepted.pending.connection.closed().await.into())
    }
    async fn accept_control(&self) -> Result<(PacketWriter, PacketReader)> {
        let (send, receive) = self.accepted.pending.connection.accept_bi().await?;
        Ok((PacketWriter(send), PacketReader(receive)))
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
        let peer = {
            let _executor = self.io.enter();
            spawn_source_peer(
                self.host.upgrade().context("Iroh host closed")?,
                connection.clone(),
                control_send,
                control_recv,
                media,
                IrohNotifier::new(move || wake()),
                presentation,
            )
        };
        let recorder = peer.diagnostics();
        Ok(SourceChannels {
            peer,
            services: (PacketWriter(service_send), PacketReader(service_recv)),
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
