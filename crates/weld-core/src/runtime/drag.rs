//! Trusted local file offers dispatched onto the native input owner.

use crate::{
    host::{ClientRuntimeNotifier, ClientRuntimeWakeSource, client_runtime_notifier},
    server::ServerState,
};
use anyhow::{Context, Result, ensure};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Canonical, URI-encoded local paths. Preparation performs filesystem access
/// and belongs on the administrative worker, before native dispatch.
#[derive(Clone, Debug)]
pub struct FileDragPayload(pub(crate) Arc<[u8]>);

impl FileDragPayload {
    pub fn from_paths(paths: &[PathBuf]) -> Result<Self> {
        ensure!(
            !paths.is_empty() && paths.len() <= 256,
            "provide between 1 and 256 paths"
        );
        let mut body = String::new();
        for path in paths {
            ensure!(path.is_absolute(), "file drag paths must be absolute");
            let canonical = path
                .canonicalize()
                .with_context(|| format!("cannot offer {}", path.display()))?;
            let uri = url::Url::from_file_path(&canonical)
                .map_err(|()| anyhow::anyhow!("cannot encode file path"))?;
            body.push_str(uri.as_str());
            body.push_str("\r\n");
            ensure!(body.len() <= 64 * 1024, "file drag URI list exceeds 64 KiB");
        }
        Ok(Self(Arc::from(body.into_bytes())))
    }
}

pub enum DragRequest {
    Begin(FileDragPayload),
    Cancel,
}

struct Pending {
    request: DragRequest,
    deadline: Instant,
    reply: SyncSender<Result<(), String>>,
}

/// Cross-thread capability for a same-user control worker.
#[derive(Clone)]
pub struct DragController {
    sender: SyncSender<Pending>,
    wake: ClientRuntimeNotifier,
}

impl DragController {
    pub fn request(&self, request: DragRequest) -> Result<()> {
        let (reply, response) = mpsc::sync_channel(1);
        self.sender
            .try_send(Pending {
                request,
                deadline: Instant::now() + REQUEST_TIMEOUT,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("native drag service busy or unavailable"))?;
        self.wake
            .notify()
            .context("could not wake drag service; outcome unknown")?;
        response
            .recv_timeout(REQUEST_TIMEOUT)
            .context("drag reply unavailable; use drag --cancel before retrying")?
            .map_err(anyhow::Error::msg)
    }
}

pub struct DragRequests {
    receiver: Receiver<Pending>,
    source: ClientRuntimeWakeSource,
}

pub fn drag_channel() -> std::io::Result<(DragController, DragRequests)> {
    let (wake, source) = client_runtime_notifier()?;
    let (sender, receiver) = mpsc::sync_channel(8);
    Ok((
        DragController { sender, wake },
        DragRequests { receiver, source },
    ))
}

pub(crate) struct DragQueue(Receiver<Pending>);
impl DragRequests {
    pub(crate) fn into_parts(self) -> (DragQueue, ClientRuntimeWakeSource) {
        (DragQueue(self.receiver), self.source)
    }
}
impl DragQueue {
    pub(crate) fn service(&self, server: &mut ServerState) {
        for pending in self.0.try_iter().take(8) {
            let result = if Instant::now() >= pending.deadline {
                Err(anyhow::anyhow!("drag request expired before dispatch"))
            } else {
                match pending.request {
                    DragRequest::Begin(payload) => server.begin_file_drag(payload),
                    DragRequest::Cancel => {
                        server.cancel_file_drag();
                        Ok(())
                    }
                }
            };
            let _ = pending
                .reply
                .send(result.map_err(|error| format!("{error:#}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_paths_are_canonical_and_uri_escaped() {
        let dir = tempfile::tempdir().expect("directory");
        let path = dir.path().join("a #é.txt");
        std::fs::write(&path, b"data").expect("file");
        let payload = FileDragPayload::from_paths(std::slice::from_ref(&path)).expect("payload");
        let uri = std::str::from_utf8(&payload.0).expect("URI");
        assert!(uri.ends_with("/a%20%23%C3%A9.txt\r\n"));
        assert_eq!(
            url::Url::parse(uri.trim())
                .expect("url")
                .to_file_path()
                .expect("path"),
            path
        );
        assert!(FileDragPayload::from_paths(&[]).is_err());
        assert!(FileDragPayload::from_paths(&[dir.path().join("missing")]).is_err());
    }
}
