//! Bounded Sway IPC binding-mode status for desktop bars and local tools.
//!
//! The compositor publishes owned snapshots. A dedicated asynchronous worker
//! serves local sockets and isolates slow or malformed clients from input work.

mod codec;
mod connection;
mod protocol;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener as StdListener},
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
};
use tempfile::TempDir;
use tokio::{
    net::UnixListener,
    runtime::Builder,
    sync::{oneshot, watch},
    task::JoinSet,
};

const MAX_CLIENTS: usize = 32;

/// Current mode and names available after a successful configuration load.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModeSnapshot {
    pub name: String,
    pub pango_markup: bool,
    pub names: Vec<String>,
}

impl Default for ModeSnapshot {
    fn default() -> Self {
        Self {
            name: "default".into(),
            pango_markup: false,
            names: vec!["default".into()],
        }
    }
}

/// Publishes the latest mode and catalog to connected status consumers.
#[derive(Clone)]
pub struct ModePublisher(watch::Sender<ModeSnapshot>);

impl ModePublisher {
    pub fn publish(&self, snapshot: ModeSnapshot) {
        self.0.send_if_modified(|current| {
            if *current == snapshot {
                return false;
            }
            *current = snapshot;
            true
        });
    }
}

/// Owns a private socket directory, worker, and bounded client lifetime.
pub struct ModeService {
    directory: TempDir,
    publisher: ModePublisher,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

/// Socket reservation before the host establishes its signal mask. Start the
/// worker after native host preparation so it inherits the session's mask.
pub struct PreparedModeService {
    directory: TempDir,
    listener: StdListener,
}

impl PreparedModeService {
    /// Reserve a private IPC socket in the user's XDG runtime directory.
    pub fn bind() -> Result<Self> {
        let directories = xdg::BaseDirectories::new();
        Self::bind_in(directories.get_runtime_directory()?)
    }

    fn bind_in(parent: &Path) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("weld-sway-")
            .tempdir_in(parent)?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        let socket = directory.path().join("ipc.sock");
        let listener = StdListener::bind(&socket).context("binding Weld Sway IPC socket")?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            directory,
            listener,
        })
    }

    pub fn socket_path(&self) -> PathBuf {
        self.directory.path().join("ipc.sock")
    }

    /// Start serving after the host has established its thread signal mask.
    /// Dropping the returned service stops clients and removes its socket.
    pub fn start(self) -> Result<ModeService> {
        let runtime = Builder::new_current_thread().enable_all().build()?;
        let listener = {
            let _guard = runtime.enter();
            UnixListener::from_std(self.listener)?
        };
        let (sender, receiver) = watch::channel(ModeSnapshot::default());
        let (stop, stopped) = oneshot::channel();
        let worker = thread::Builder::new()
            .name("weld-sway-ipc".into())
            .spawn(move || {
                runtime.block_on(serve(listener, receiver, stopped));
            })?;
        Ok(ModeService {
            directory: self.directory,
            publisher: ModePublisher(sender),
            stop: Some(stop),
            worker: Some(worker),
        })
    }
}

impl ModeService {
    pub fn socket_path(&self) -> PathBuf {
        self.directory.path().join("ipc.sock")
    }
    pub fn publisher(&self) -> ModePublisher {
        self.publisher.clone()
    }
}

impl Drop for ModeService {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::warn!("Sway IPC worker failed during shutdown");
        }
    }
}

async fn serve(
    listener: UnixListener,
    snapshot: watch::Receiver<ModeSnapshot>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = clients.join_next(), if !clients.is_empty() => {},
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::warn!(%error, "Sway IPC accept failed");
                        break;
                    }
                };
                let same_user = stream.peer_cred()
                    .is_ok_and(|peer| peer.uid() == rustix::process::geteuid().as_raw());
                if clients.len() >= MAX_CLIENTS || !same_user {
                    continue;
                }
                let snapshot = snapshot.clone();
                clients.spawn(async move {
                    if let Err(error) = connection::run(stream, snapshot).await {
                        tracing::debug!(%error, "closed Sway IPC client");
                    }
                });
            }
        }
    }
    clients.abort_all();
    while clients.join_next().await.is_some() {}
}
