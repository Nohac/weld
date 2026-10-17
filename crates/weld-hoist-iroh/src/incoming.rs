//! Incoming authenticated Iroh connections exposed to the shared session runtime.
use crate::admission::{Accepted as SourceAccepted, PendingConnection};
use crate::connection::IrohConnection;
use anyhow::{Context, Result};
use iroh::{Endpoint, endpoint::IncomingAddr};
use std::{
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::task::{JoinHandle, JoinSet};
use weld_session::{Accepted, Listener};

pub(crate) struct IrohListener {
    incoming: async_channel::Receiver<Result<Accepted<IrohConnection>>>,
    task: JoinHandle<()>,
}
impl IrohListener {
    pub fn new(endpoint: Endpoint, owner: Arc<Mutex<Weak<crate::host::HostLifetime>>>) -> Self {
        let (sender, incoming) = async_channel::bounded(8);
        // Connection-driver tasks stay on the endpoint's transport executor.
        let io = tokio::runtime::Handle::current();
        let task = tokio::spawn(async move {
            let mut handshakes = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = handshakes.join_next(), if !handshakes.is_empty() => {
                        let Some(accepted) = accepted else { continue; };
                        let result = (|| {
                            let (pending, adb_route): (PendingConnection, _) = accepted.context("Iroh handshake task failed")??;
                            let protocol = std::str::from_utf8(pending.connection.alpn())?.to_owned();
                            let host = owner.lock().map_err(|_| anyhow::anyhow!("Iroh owner unavailable"))?.clone();
                            Ok(Accepted { protocol, connection: IrohConnection { accepted: SourceAccepted { pending, adb_route }, host, io: io.clone() } })
                        })();
                        if sender.try_send(result).is_err() && sender.is_closed() { break; }
                    },
                    incoming = endpoint.accept(), if handshakes.len() < 8 => {
                        let Some(incoming) = incoming else { break; };
                        handshakes.spawn(async move {
                            let route = match incoming.remote_addr() { IncomingAddr::Custom(address) => Some(address), _ => None };
                            let connection = tokio::time::timeout(Duration::from_secs(5), incoming).await??;
                            Ok::<_, anyhow::Error>((PendingConnection::new(connection), route))
                        });
                    }
                }
            }
        });
        Self { incoming, task }
    }
}
impl Drop for IrohListener {
    fn drop(&mut self) {
        self.incoming.close();
        self.task.abort();
        while self.incoming.try_recv().is_ok() {}
    }
}
impl Listener for IrohListener {
    type Connection = IrohConnection;
    async fn accept(&mut self) -> Option<Result<Accepted<IrohConnection>>> {
        self.incoming.recv().await.ok()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    const WELD_ALPN: &[u8] = weld_hoist_core::session::server::DIRECT_SERVICE.as_bytes();
    const PAIR_ALPN: &[u8] = weld_hoist_core::session::server::ENROLLMENT_SERVICE.as_bytes();
    use iroh::{EndpointAddr, RelayMode, endpoint::presets};
    use std::net::Ipv4Addr;
    use weld_hoist_core::session::server::SessionRuntime;

    #[tokio::test]
    async fn stalled_pairing_connections_do_not_starve_media_admission() {
        let source = Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("bind")
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![WELD_ALPN.to_vec(), PAIR_ALPN.to_vec()])
            .bind()
            .await
            .expect("source");
        let client = Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("bind")
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .expect("client");
        let runtime = SessionRuntime::start().expect("runtime");
        let pairing = crate::pairing::PairingHost::default();
        let directory = crate::rendezvous::tests::ExchangeDirectory::new();
        pairing.enable(&directory.0).expect("authority");
        let mut registry = weld_session::Registry::default();
        let hub = weld_hoist_core::session::server::register(
            &mut registry,
            crate::connection::HostServices(pairing),
        )
        .expect("services");
        runtime
            .listen(
                IrohListener::new(source.clone(), Arc::new(Mutex::new(Weak::new()))),
                registry,
            )
            .expect("listener");
        let address = EndpointAddr::new(source.id()).with_ip_addr(source.bound_sockets()[0]);
        let mut stalled = Vec::new();
        for _ in 0..8 {
            stalled.push(
                client
                    .connect(address.clone(), PAIR_ALPN)
                    .await
                    .expect("pairing handshake"),
            );
        }
        let media = client
            .connect(address, WELD_ALPN)
            .await
            .expect("media handshake");
        let admitted = tokio::time::timeout(Duration::from_secs(1), async { hub.recv().await })
            .await
            .expect("independent admission deadline")
            .expect("media admitted");
        assert_eq!(
            admitted.accepted.pending.connection.remote_id(),
            client.id()
        );
        drop(admitted);
        drop(media);
        drop(stalled);
        drop(hub);
        drop(runtime);
        source.close().await;
        client.close().await;
    }
}
