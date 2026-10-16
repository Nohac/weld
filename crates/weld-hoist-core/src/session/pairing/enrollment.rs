//! Ordered enrollment exchange and confirmation deadline shared by listeners.

use super::{AuthenticatedPeer, INVITATION_LIFETIME, PairingAuthority, validate_name};
use crate::session::{
    channel::{receive_record, send_record},
    services::ServiceTransport,
};
use anyhow::{Context, Result, ensure};
use async_io::Timer;
use futures_lite::future::race;
use std::{
    future::Future,
    time::{Duration, Instant},
};
use weld_hoist_protocol::device::{DevicePermissions, PairReply, PairRequest};

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

async fn until<T>(deadline: Instant, operation: impl Future<Output = Result<T>>) -> Result<T> {
    race(operation, async {
        Timer::at(deadline).await;
        anyhow::bail!("pairing exchange timed out")
    })
    .await
}

pub async fn serve(
    authority: &PairingAuthority,
    peer: AuthenticatedPeer,
    lane: &mut impl ServiceTransport,
) -> Result<()> {
    let deadline = Instant::now() + INVITATION_LIFETIME;
    let request: PairRequest = until(
        Instant::now() + Duration::from_secs(5),
        receive_record(lane),
    )
    .await?;
    let claim = authority
        .claim(peer.clone(), request.token, request.name)?
        .wait()
        .await??;
    until(
        deadline.min(claim.expires),
        send_record(lane, &PairReply::Verify(claim.candidate.verification)),
    )
    .await?;
    ensure!(
        until(deadline.min(claim.expires), claim.approval.wait()).await?,
        "pairing cancelled"
    );
    let permissions = authority
        .permissions(&peer)
        .context("device approval revoked")?;
    until(
        Instant::now() + Duration::from_secs(5),
        send_record(lane, &PairReply::Approved(permissions)),
    )
    .await
}

pub async fn enroll(
    lane: &mut impl ServiceTransport,
    token: [u8; 32],
    name: String,
    host: String,
    progress: impl Fn(PairingProgress) -> Result<()>,
) -> Result<()> {
    validate_name(&name)?;
    send_record(lane, &PairRequest { token, name }).await?;
    let PairReply::Verify(code) = receive_record(lane).await? else {
        anyhow::bail!("invalid pairing response");
    };
    ensure!(
        code.len() == 6 && code.bytes().all(|value| value.is_ascii_hexdigit()),
        "invalid verification code"
    );
    progress(PairingProgress::Verify {
        host: host.clone(),
        code,
    })?;
    let PairReply::Approved(permissions) = receive_record(lane).await? else {
        anyhow::bail!("invalid approval response");
    };
    progress(PairingProgress::Approved { host, permissions })
}
