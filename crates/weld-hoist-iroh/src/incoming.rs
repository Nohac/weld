//! Bounded ALPN dispatch for the endpoint's media and device-control protocols.
use crate::{
    admission::{Accepted, IncomingQueue, PendingConnection, WELD_ALPN},
    pairing::{self, PairingHost},
};
use iroh::{Endpoint, endpoint::IncomingAddr};
use std::{
    sync::{Arc, Mutex as StdMutex, Weak},
    time::Duration,
};
use tokio::{
    sync::{Mutex, mpsc},
    task::{JoinHandle, JoinSet},
};

pub(crate) struct IncomingHub {
    pub hoist: IncomingQueue,
    task: JoinHandle<()>,
}
impl IncomingHub {
    pub fn start(
        endpoint: Endpoint,
        pairing: PairingHost,
        owner: Arc<StdMutex<Weak<crate::host::HostLifetime>>>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            let mut handshakes = JoinSet::new();
            let mut invitations = JoinSet::new();
            let mut sessions = JoinSet::new();
            loop {
                tokio::select! {
                    _ = invitations.join_next(), if !invitations.is_empty() => {},
                    _ = sessions.join_next(), if !sessions.is_empty() => {},
                    accepted = handshakes.join_next(), if !handshakes.is_empty() => {
                        let Some(Ok(Some(Accepted { pending: guard, adb_route }))) = accepted else { continue; };
                        match guard.connection.alpn() {
                            WELD_ALPN => { let _ = sender.try_send(Accepted { pending: guard, adb_route }); }
                            pairing::PAIR_ALPN if invitations.len() < 8 => {
                                let pairing = pairing.clone();
                                invitations.spawn(async move {
                                    let _ = pairing::accept(pairing, guard.connection.clone()).await;
                                });
                            }
                            pairing::SESSION_ALPN if sessions.len() < 8 => {
                                let pairing = pairing.clone();
                                let host = owner.lock().map(|owner| owner.clone()).unwrap_or_default();
                                sessions.spawn(async move {
                                    if let Err(error) = pairing::accept_session(host, pairing, guard.connection.clone()).await {
                                        tracing::debug!(%error, "device connection ended");
                                    }
                                });
                            }
                            _ => {}
                        }
                    },
                    incoming = endpoint.accept(), if handshakes.len() < 8 => {
                        let Some(incoming) = incoming else { break; };
                        handshakes.spawn(async move {
                            let route = match incoming.remote_addr() { IncomingAddr::Custom(address) => Some(address), _ => None };
                            let Ok(Ok(connection)) = tokio::time::timeout(Duration::from_secs(5), incoming).await else { return None; };
                            let guard = PendingConnection::new(connection);
                            Some(Accepted { pending: guard, adb_route: route })
                        });
                    }
                }
            }
        });
        Self {
            hoist: Arc::new(Mutex::new(receiver)),
            task,
        }
    }
}
impl Drop for IncomingHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{EndpointAddr, RelayMode, endpoint::presets};
    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn stalled_pairing_connections_do_not_starve_media_admission() {
        let source = Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("bind")
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![WELD_ALPN.to_vec(), pairing::PAIR_ALPN.to_vec()])
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
        let hub = IncomingHub::start(
            source.clone(),
            PairingHost::default(),
            Arc::new(StdMutex::new(Weak::new())),
        );
        let address = EndpointAddr::new(source.id()).with_ip_addr(source.bound_sockets()[0]);
        let mut stalled = Vec::new();
        for _ in 0..8 {
            stalled.push(
                client
                    .connect(address.clone(), pairing::PAIR_ALPN)
                    .await
                    .expect("pairing handshake"),
            );
        }
        let media = client
            .connect(address, WELD_ALPN)
            .await
            .expect("media handshake");
        let admitted = tokio::time::timeout(Duration::from_secs(1), async {
            hub.hoist.lock().await.recv().await
        })
        .await
        .expect("independent admission deadline")
        .expect("media admitted");
        assert_eq!(admitted.pending.connection.remote_id(), client.id());
        drop(admitted);
        drop(media);
        drop(stalled);
        drop(hub);
        source.close().await;
        client.close().await;
    }
}
