//! Bounded packet interfaces used by shared session drivers.

use anyhow::{Context, Result, ensure};
use async_io::Timer;
use futures_lite::future::race;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    future::Future,
    pin::pin,
    time::{Duration, Instant},
};
use weld_diagnostics::{Cause, Observation, Operation, Recorder};

const HEADER_BYTES: usize = 192 * 1024;

/// Ordered packet writes. Dropping the owner cancels an incomplete transfer.
pub trait PacketSend: Send {
    fn send(&mut self, packet: &[u8]) -> impl Future<Output = Result<()>> + Send;
}

/// Ordered packet reads, bounded before allocating payload storage.
pub trait PacketReceive: Send {
    fn receive(&mut self, maximum: usize) -> impl Future<Output = Result<Vec<u8>>> + Send;
}

pub(crate) async fn send_record(lane: &mut impl PacketSend, value: &impl Serialize) -> Result<()> {
    let bytes = postcard::to_allocvec(value)?;
    ensure!(bytes.len() <= HEADER_BYTES, "service header exceeds bound");
    lane.send(&bytes).await
}

pub(crate) async fn receive_record<T: DeserializeOwned>(
    lane: &mut impl PacketReceive,
) -> Result<T> {
    postcard::from_bytes(&lane.receive(HEADER_BYTES).await?).context("invalid service record")
}

/// A delayed live control operation retains its partially transferred record.
/// The driver owner cancels it when the connection or session ends.
pub async fn retain_operation<T>(
    warn_after: Duration,
    operation: Operation,
    recorder: Option<&Recorder>,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    let started = Instant::now();
    let mut future = pin!(future);
    let result = match race(async { Some(future.as_mut().await) }, async {
        Timer::after(warn_after).await;
        None
    })
    .await
    {
        Some(result) => result,
        None => {
            tracing::warn!(
                ?operation,
                "device session control exchange delayed; retaining pending operation"
            );
            let result = future.await;
            if result.is_ok() {
                tracing::info!(
                    ?operation,
                    elapsed_ms = started.elapsed().as_millis(),
                    "device session control exchange resumed"
                );
            }
            result
        }
    };
    if result.is_err()
        && let Some(recorder) = recorder
    {
        recorder.record(Observation::Failure {
            operation,
            cause: Cause::ProtocolOrIo,
        });
    }
    result.with_context(|| format!("device session {operation:?} failed"))
}
