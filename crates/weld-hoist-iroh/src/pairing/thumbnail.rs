//! Bounded preview cache and independent per-device transfer lane.

use super::{DesktopSessions, SessionId};
use crate::{
    IrohNotifier,
    framing::{read_record, write_record},
};
use anyhow::{Context, Result, ensure};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
pub use weld_hoist_protocol::thumbnail::{
    ThumbnailFreshness, ThumbnailImageHeader, ThumbnailReply, ThumbnailRequest, ThumbnailRevision,
    ThumbnailStatus, ThumbnailTarget,
};

pub(super) const MARKER: &[u8; 8] = b"weldthm1";
const CACHE_ENTRIES: usize = 32;
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);

/// Encoded PNG storage shared across clients. Consumers must bound image decoding
/// to the advertised dimensions and verify the decoded extent matches the header.
#[derive(Clone, Debug)]
pub struct ThumbnailImage {
    header: ThumbnailImageHeader,
    png: Arc<[u8]>,
}
impl ThumbnailImage {
    pub fn new(width: u32, height: u32, png: Vec<u8>) -> Result<Self> {
        let header = ThumbnailImageHeader {
            width,
            height,
            png_bytes: u32::try_from(png.len())?,
        };
        header.validate().map_err(anyhow::Error::msg)?;
        Ok(Self {
            header,
            png: png.into(),
        })
    }
    pub fn header(&self) -> ThumbnailImageHeader {
        self.header
    }
    pub fn png(&self) -> &[u8] {
        &self.png
    }
}

#[derive(Clone, Debug)]
pub struct ThumbnailResponse {
    pub reply: ThumbnailReply,
    pub image: Option<ThumbnailImage>,
}
impl ThumbnailResponse {
    fn status(target: ThumbnailTarget, status: ThumbnailStatus) -> Self {
        Self {
            reply: ThumbnailReply { target, status },
            image: None,
        }
    }
}

/// One capture ticket. Completion is accepted only while this ticket is current.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThumbnailCapture {
    pub target: ThumbnailTarget,
    revision: ThumbnailRevision,
}
struct Snapshot {
    revision: ThumbnailRevision,
    image: ThumbnailImage,
    captured: Instant,
}
struct Entry {
    snapshot: Option<Snapshot>,
    dirty: bool,
    requested: bool,
    last_attempt: Option<Instant>,
    last_used: Instant,
}
#[derive(Default)]
pub(super) struct Cache {
    entries: BTreeMap<ThumbnailTarget, Entry>,
    active: Option<(ThumbnailCapture, Instant)>,
    next: u64,
}
impl Cache {
    pub(super) fn retain(&mut self, mut visible: impl FnMut(ThumbnailTarget) -> bool) {
        self.entries.retain(|target, _| visible(*target));
        if self
            .active
            .is_some_and(|(capture, _)| !visible(capture.target))
        {
            self.active = None;
        }
    }
    fn request(&mut self, request: ThumbnailRequest, now: Instant) -> ThumbnailResponse {
        if !self.entries.contains_key(&request.target) && self.entries.len() == CACHE_ENTRIES {
            let oldest = self
                .entries
                .iter()
                .filter(|(target, _)| {
                    self.active
                        .is_none_or(|(capture, _)| capture.target != **target)
                })
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(target, _)| *target);
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        let entry = self.entries.entry(request.target).or_insert(Entry {
            snapshot: None,
            dirty: true,
            requested: false,
            last_attempt: None,
            last_used: now,
        });
        entry.last_used = now;
        entry.requested |= entry.dirty
            || (entry.snapshot.is_none()
                && self
                    .active
                    .is_none_or(|(capture, _)| capture.target != request.target));
        let Some(snapshot) = &entry.snapshot else {
            return ThumbnailResponse::status(request.target, ThumbnailStatus::Pending);
        };
        let freshness = ThumbnailFreshness {
            age_ms: u64::try_from(now.saturating_duration_since(snapshot.captured).as_millis())
                .unwrap_or(u64::MAX),
            stale: entry.dirty
                || self
                    .active
                    .is_some_and(|(capture, _)| capture.target == request.target),
        };
        if request.if_revision == Some(snapshot.revision) {
            ThumbnailResponse::status(
                request.target,
                ThumbnailStatus::Unchanged {
                    revision: snapshot.revision,
                    freshness,
                },
            )
        } else {
            ThumbnailResponse {
                reply: ThumbnailReply {
                    target: request.target,
                    status: ThumbnailStatus::Image {
                        revision: snapshot.revision,
                        freshness,
                        image: snapshot.image.header,
                    },
                },
                image: Some(snapshot.image.clone()),
            }
        }
    }
    fn take(&mut self, now: Instant) -> Option<ThumbnailCapture> {
        if let Some((capture, started)) = self.active {
            if now.saturating_duration_since(started) < CAPTURE_TIMEOUT {
                return None;
            }
            self.active = None;
            if let Some(entry) = self.entries.get_mut(&capture.target) {
                entry.dirty = true;
                entry.requested = true;
            }
        }
        let (&target, entry) = self
            .entries
            .iter_mut()
            .filter(|(_, entry)| {
                entry.requested
                    && entry
                        .last_attempt
                        .is_none_or(|last| now.saturating_duration_since(last) >= REFRESH_INTERVAL)
            })
            .min_by_key(|(_, entry)| entry.last_attempt)?;
        self.next = self.next.checked_add(1)?;
        let capture = ThumbnailCapture {
            target,
            revision: ThumbnailRevision(self.next),
        };
        entry.requested = false;
        entry.dirty = false;
        entry.last_attempt = Some(now);
        self.active = Some((capture, now));
        Some(capture)
    }
    fn complete(
        &mut self,
        capture: ThumbnailCapture,
        image: Option<ThumbnailImage>,
        now: Instant,
    ) -> bool {
        if self.active.is_none_or(|(active, started)| {
            active != capture || now.saturating_duration_since(started) >= CAPTURE_TIMEOUT
        }) {
            return false;
        }
        self.active = None;
        let Some(entry) = self.entries.get_mut(&capture.target) else {
            return false;
        };
        if let Some(image) = image {
            entry.snapshot = Some(Snapshot {
                revision: capture.revision,
                image,
                captured: now,
            });
            entry.requested &= entry.dirty;
        } else {
            entry.dirty = true;
        }
        true
    }
}

impl DesktopSessions {
    /// Called by the capture producer on composition changes; coalesces dirty state.
    pub fn invalidate_thumbnail(&self, target: ThumbnailTarget) {
        if let Ok(mut state) = self.state.lock()
            && let Some(entry) = state.thumbnails.entries.get_mut(&target)
        {
            entry.dirty = true;
        }
    }
    /// At most one capture is active for the entire desktop. Call from the producer's
    /// ordinary servicing loop; capture itself must preserve dormant frame demand.
    pub fn take_thumbnail_capture(&self) -> Option<ThumbnailCapture> {
        let mut state = self.state.lock().ok()?;
        state.prune_thumbnails();
        state.thumbnails.take(Instant::now())
    }
    /// Publish asynchronous capture completion, or `None` on failure. A late result
    /// for a removed/replaced occupant or expired ticket is discarded.
    pub fn complete_thumbnail_capture(
        &self,
        capture: ThumbnailCapture,
        image: Option<ThumbnailImage>,
    ) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        state.prune_thumbnails();
        state.thumbnails.complete(capture, image, Instant::now())
    }
    fn thumbnail(&self, session: SessionId, request: ThumbnailRequest) -> ThumbnailResponse {
        let Ok(mut state) = self.state.lock() else {
            return ThumbnailResponse::status(request.target, ThumbnailStatus::Unavailable);
        };
        state.prune_thumbnails();
        let visible = state
            .catalogues
            .get(&session)
            .is_some_and(|(apps, access)| {
                access.is_authorized()
                    && apps.iter().any(|app| {
                        app.window == request.target.window && app.surface == request.target.surface
                    })
            });
        if !visible {
            return ThumbnailResponse::status(request.target, ThumbnailStatus::Unavailable);
        }
        let response = state.thumbnails.request(request, Instant::now());
        drop(state);
        let _ = self.policy.notify();
        response
    }
}

pub(super) async fn serve(
    desktop: DesktopSessions,
    session: SessionId,
    authority: super::PairingHost,
    connection: Connection,
    mut send: SendStream,
    mut recv: RecvStream,
) {
    let serving = async {
        loop {
            let request: ThumbnailRequest = match read_record(&mut recv).await {
                Ok(request) => request,
                Err(error) => break Err::<(), _>(error),
            };
            let response = if authority.permits_thumbnails(connection.remote_id()) {
                desktop.thumbnail(session, request)
            } else {
                ThumbnailResponse::status(request.target, ThumbnailStatus::Denied)
            };
            // A slow preview owns only this lane. Dropping it cancels its partial record.
            tokio::time::timeout(TRANSFER_TIMEOUT, write_response(&mut send, &response)).await??;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    tokio::select! {
        result = serving => if let Err(error) = result { tracing::debug!(%error, "thumbnail lane ended"); },
        _ = connection.closed() => {},
    }
}

/// One bounded response; polling is independent of the application catalogue.
pub struct PendingThumbnail(oneshot::Receiver<Result<ThumbnailResponse, String>>);
impl PendingThumbnail {
    pub fn poll(&mut self) -> Result<Option<ThumbnailResponse>> {
        match self.0.try_recv() {
            Ok(result) => result.map(Some).map_err(anyhow::Error::msg),
            Err(oneshot::error::TryRecvError::Empty) => Ok(None),
            Err(_) => anyhow::bail!("thumbnail lane stopped"),
        }
    }
}
type Command = (
    ThumbnailRequest,
    oneshot::Sender<Result<ThumbnailResponse, String>>,
);
pub(super) struct Client {
    commands: mpsc::Sender<Command>,
    task: JoinHandle<()>,
}
impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Client {
    pub(super) fn start(
        connection: Connection,
        mut send: SendStream,
        mut recv: RecvStream,
        notifier: IrohNotifier,
    ) -> Self {
        let (commands, mut requests) = mpsc::channel::<Command>(1);
        let task = tokio::spawn(async move {
            loop {
                let command = tokio::select! {
                    command = requests.recv() => command,
                    _ = connection.closed() => break,
                };
                let Some((request, answer)) = command else {
                    break;
                };
                if answer.is_closed() {
                    continue;
                }
                let exchange = async {
                    write_record(&mut send, &request).await?;
                    read_response(&mut recv, request).await
                };
                let result = tokio::select! {
                    result = tokio::time::timeout(TRANSFER_TIMEOUT, exchange) => result.context("thumbnail transfer timed out").and_then(|result| result),
                    _ = connection.closed() => break,
                };
                let failed = result.is_err();
                let _ = answer.send(result.map_err(|error| format!("{error:#}")));
                let _ = notifier.notify();
                if failed {
                    break;
                }
            }
        });
        Self { commands, task }
    }
    pub(super) fn request(&self, request: ThumbnailRequest) -> Result<PendingThumbnail> {
        let (answer, wait) = oneshot::channel();
        self.commands
            .try_send((request, answer))
            .map_err(|_| anyhow::anyhow!("thumbnail queue busy or unavailable"))?;
        Ok(PendingThumbnail(wait))
    }
}

async fn write_response(
    writer: &mut (impl AsyncWrite + Unpin),
    response: &ThumbnailResponse,
) -> Result<()> {
    write_record(writer, &response.reply).await?;
    if let Some(image) = &response.image {
        writer.write_all(image.png()).await?;
    }
    Ok(())
}
async fn read_response(
    reader: &mut (impl AsyncRead + Unpin),
    request: ThumbnailRequest,
) -> Result<ThumbnailResponse> {
    let reply: ThumbnailReply = read_record(reader).await?;
    ensure!(reply.target == request.target, "thumbnail target mismatch");
    let image = match reply.status {
        ThumbnailStatus::Image { image, .. } => {
            image.validate().map_err(anyhow::Error::msg)?;
            let mut png = vec![0; usize::try_from(image.png_bytes)?];
            reader.read_exact(&mut png).await?;
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
