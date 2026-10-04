use super::*;
use crate::framing::{read_record, write_record};
use iroh::Endpoint;
use tokio::time::timeout_at;

pub(crate) const PAIR_ALPN: &[u8] = b"weld/pair/1";
pub(crate) const SESSION_ALPN: &[u8] = b"weld/device/1";

#[derive(Serialize, Deserialize)]
struct PairRequest {
    token: [u8; 32],
    name: String,
}
#[derive(Serialize, Deserialize)]
enum PairReply {
    Verify(String),
    Approved(DevicePermissions),
}

pub(crate) async fn accept(host: PairingHost, connection: Connection) -> Result<()> {
    let deadline = Instant::now() + INVITATION_LIFETIME;
    let (mut send, mut recv) = timeout_at(
        (Instant::now() + Duration::from_secs(5)).into(),
        connection.accept_bi(),
    )
    .await??;
    let request: PairRequest = timeout_at(
        (Instant::now() + Duration::from_secs(5)).into(),
        read_record(&mut recv),
    )
    .await??;
    let (candidate, approval, expires) =
        host.claim(connection.remote_id(), request.token, request.name)?;
    write_record(&mut send, &PairReply::Verify(candidate.verification)).await?;
    let approved = tokio::select! {
        _ = connection.closed() => false,
        result = timeout_at(deadline.min(expires).into(), approval) => matches!(result, Ok(Ok(true))),
    };
    ensure!(approved, "pairing cancelled or expired");
    let permissions = host.authorize(&connection)?;
    write_record(&mut send, &PairReply::Approved(permissions)).await?;
    send.finish()?;
    timeout_at(
        (Instant::now() + Duration::from_secs(5)).into(),
        send.stopped(),
    )
    .await??;
    Ok(())
}

/// Enrollment progress exposed without networking types or secret credentials.
#[derive(Clone, Debug)]
pub enum PairingProgress {
    Connecting,
    Verify {
        host: String,
        code: String,
    },
    Approved {
        host: String,
        permissions: DevicePermissions,
    },
    Failed(String),
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
    validate_name(&name)?;
    let connection = endpoint
        .connect(invitation.profile()?.endpoint_addr()?, PAIR_ALPN)
        .await?;
    let guard = crate::admission::PendingConnection::new(connection);
    let (mut send, mut recv) = guard.connection.open_bi().await?;
    write_record(
        &mut send,
        &PairRequest {
            token: invitation.token,
            name,
        },
    )
    .await?;
    let PairReply::Verify(code) = read_record(&mut recv).await? else {
        anyhow::bail!("invalid pairing response");
    };
    ensure!(
        code.len() == 6 && code.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid verification code"
    );
    *progress
        .lock()
        .map_err(|_| anyhow::anyhow!("pairing state poisoned"))? = PairingProgress::Verify {
        host: invitation.name.clone(),
        code,
    };
    notifier.notify()?;
    let PairReply::Approved(permissions) = read_record(&mut recv).await? else {
        anyhow::bail!("invalid approval response");
    };
    *progress
        .lock()
        .map_err(|_| anyhow::anyhow!("pairing state poisoned"))? = PairingProgress::Approved {
        host: invitation.name,
        permissions,
    };
    notifier.notify()?;
    Ok(())
}
