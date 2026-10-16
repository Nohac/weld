//! Enrollment stream setup and lifetime on an authenticated endpoint.
use super::*;
use crate::channel::PacketLane;
pub use enrollment::PairingProgress;
use iroh::Endpoint;
use weld_hoist_core::session::channel::PacketSend;
use weld_hoist_core::session::pairing::enrollment;

pub(crate) const PAIR_ALPN: &[u8] = b"weld/pair/1";
pub(crate) const SESSION_ALPN: &[u8] = b"weld/device/1";

pub(crate) async fn accept(host: PairingHost, connection: Connection) -> Result<()> {
    let (send, recv) =
        tokio::time::timeout(Duration::from_secs(5), connection.accept_bi()).await??;
    let mut lane = PacketLane::new(send, recv);
    tokio::select! {
        result = enrollment::serve(host.authority()?, authenticated(connection.remote_id())?, &mut lane) => result?,
        reason = connection.closed() => return Err(reason.into()),
    }
    tokio::time::timeout(Duration::from_secs(5), lane.finish()).await??;
    Ok(())
}

pub struct PendingPairing {
    pub(crate) progress: Arc<Mutex<PairingProgress>>,
    pub(crate) cancel: Option<oneshot::Sender<()>>,
}
impl PendingPairing {
    pub fn progress(&self) -> PairingProgress {
        self.progress
            .lock()
            .map(|value| value.clone())
            .unwrap_or_else(|_| PairingProgress::Failed("pairing state unavailable".into()))
    }
}
impl Drop for PendingPairing {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

pub(crate) async fn connect(
    endpoint: Endpoint,
    invitation: PairingInvitation,
    name: String,
    progress: Arc<Mutex<PairingProgress>>,
    notifier: IrohNotifier,
) -> Result<()> {
    let connection = endpoint
        .connect(invitation.profile()?.endpoint_addr()?, PAIR_ALPN)
        .await?;
    let guard = crate::admission::PendingConnection::new(connection);
    let (send, recv) = guard.connection.open_bi().await?;
    enrollment::enroll(
        &mut PacketLane::new(send, recv),
        invitation.token,
        name,
        invitation.name,
        |value| {
            *progress
                .lock()
                .map_err(|_| anyhow::anyhow!("pairing progress unavailable"))? = value;
            notifier.notify()?;
            Ok(())
        },
    )
    .await
}
