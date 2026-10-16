//! Shared service-lane dispatch, authorization and request budgets.

use super::channel::{PacketReceive, PacketSend, receive_record, send_record};
use super::thumbnail::{ThumbnailImage, ThumbnailRequest, ThumbnailResponse, ThumbnailStatus};
use super::{Pending, Reply, SessionId, SessionService, reply_channel};
use anyhow::{Result, ensure};
use async_channel::{Receiver, Sender};
use async_io::Timer;
use futures_lite::future::race;
use std::{future::Future, time::Duration};
use weld_access::AccessSession;
use weld_hoist_protocol::{
    device::DevicePermissions,
    service::{ServiceReply, ServiceRequest},
    thumbnail::MAX_IMAGE_BYTES,
};

const EXCHANGE_DEADLINE: Duration = Duration::from_secs(10);
const REQUEST_INTERVAL: Duration = Duration::from_millis(250);

/// Ordered, bounded packets on one independent logical lane. Dropping a lane
/// cancels its incomplete transfer while preserving other lanes on the connection.
pub trait ServiceTransport: PacketSend + PacketReceive {}
impl<T: PacketSend + PacketReceive> ServiceTransport for T {}

pub struct ServiceSession {
    pub service: SessionService,
    pub id: SessionId,
    pub access: AccessSession,
}

impl ServiceSession {
    pub async fn serve(
        self,
        mut lane: impl ServiceTransport,
        permissions: impl Fn() -> Option<DevicePermissions> + Send + Sync,
    ) -> Result<()> {
        loop {
            let request: ServiceRequest = receive_record(&mut lane).await?;
            match request {
                ServiceRequest::Thumbnail(request) => {
                    let allowed = || {
                        self.access.is_authorized()
                            && permissions().is_some_and(|grant| grant.browse && grant.thumbnails)
                    };
                    let mut response = if allowed() {
                        match self.service.thumbnail(self.id, request) {
                            Ok(pending) => pending.wait().await?,
                            Err(_) => ThumbnailResponse::status(
                                request.target,
                                ThumbnailStatus::Unavailable,
                            ),
                        }
                    } else {
                        ThumbnailResponse::status(request.target, ThumbnailStatus::Denied)
                    };
                    if !allowed() {
                        response =
                            ThumbnailResponse::status(request.target, ThumbnailStatus::Denied);
                    }
                    bounded_exchange(write_thumbnail(&mut lane, &response)).await?;
                }
            }
            Timer::after(REQUEST_INTERVAL).await;
        }
    }
}

pub struct PendingThumbnail(Pending<Result<ThumbnailResponse, String>>);
impl PendingThumbnail {
    pub fn poll(&mut self) -> Result<Option<ThumbnailResponse>> {
        self.0.poll()?.transpose().map_err(anyhow::Error::msg)
    }
}
struct Command {
    request: ThumbnailRequest,
    answer: Reply<Result<ThumbnailResponse, String>>,
}

pub struct ServiceClient {
    commands: Sender<Command>,
}
impl Drop for ServiceClient {
    fn drop(&mut self) {
        self.commands.close();
    }
}
pub struct ServiceClientDriver {
    requests: Receiver<Command>,
    notify: Box<dyn Fn() + Send>,
}
impl ServiceClient {
    pub fn new(notify: impl Fn() + Send + 'static) -> (Self, ServiceClientDriver) {
        let (commands, requests) = async_channel::bounded(1);
        (
            Self { commands },
            ServiceClientDriver {
                requests,
                notify: Box::new(notify),
            },
        )
    }
    pub fn thumbnail(&self, request: ThumbnailRequest) -> Result<PendingThumbnail> {
        let (answer, pending) = reply_channel();
        self.commands
            .try_send(Command { request, answer })
            .map_err(|_| anyhow::anyhow!("service request queue busy or stopped"))?;
        Ok(PendingThumbnail(pending))
    }
}
impl ServiceClientDriver {
    pub async fn run(self, mut lane: impl ServiceTransport) -> Result<()> {
        while let Ok(Command { request, answer }) = self.requests.recv().await {
            if answer.is_closed() {
                continue;
            }
            let exchange = async {
                send_record(&mut lane, &ServiceRequest::Thumbnail(request)).await?;
                read_thumbnail(&mut lane, request).await
            };
            let result = bounded_exchange(exchange).await;
            let failed = result.is_err();
            let _ = answer.send(result.map_err(|error| format!("{error:#}")));
            (self.notify)();
            if failed {
                break;
            }
        }
        Ok(())
    }
}

async fn bounded_exchange<T>(exchange: impl Future<Output = Result<T>>) -> Result<T> {
    race(exchange, async {
        Timer::after(EXCHANGE_DEADLINE).await;
        anyhow::bail!("service exchange timed out")
    })
    .await
}
async fn write_thumbnail(
    lane: &mut impl ServiceTransport,
    response: &ThumbnailResponse,
) -> Result<()> {
    send_record(lane, &ServiceReply::Thumbnail(response.reply)).await?;
    if let Some(image) = &response.image {
        lane.send(image.png()).await?;
    }
    Ok(())
}
async fn read_thumbnail(
    lane: &mut impl ServiceTransport,
    request: ThumbnailRequest,
) -> Result<ThumbnailResponse> {
    let ServiceReply::Thumbnail(reply) = receive_record(lane).await?;
    ensure!(reply.target == request.target, "thumbnail target mismatch");
    let image = match reply.status {
        ThumbnailStatus::Image { image, .. } => {
            image.validate().map_err(anyhow::Error::msg)?;
            let png = lane.receive(MAX_IMAGE_BYTES).await?;
            ensure!(
                png.len() == usize::try_from(image.png_bytes)?,
                "thumbnail payload length mismatch"
            );
            Some(ThumbnailImage::new(image.width, image.height, png)?)
        }
        ThumbnailStatus::Unchanged { revision, .. } => {
            ensure!(
                request.if_revision == Some(revision),
                "thumbnail revision mismatch"
            );
            None
        }
        ThumbnailStatus::Pending | ThumbnailStatus::Unavailable | ThumbnailStatus::Denied => None,
    };
    Ok(ThumbnailResponse { reply, image })
}

#[cfg(test)]
mod tests;
