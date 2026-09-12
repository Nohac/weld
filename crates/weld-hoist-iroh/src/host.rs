//! One long-lived Iroh endpoint with disposable peer connections.

use std::{
    path::Path,
    str::FromStr,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use iroh::{Endpoint, EndpointId, RelayMode, Watcher, endpoint::presets};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::sync::{mpsc, oneshot};
use weld_hoist_encoded::EncodedSourceTransport;
use weld_media::VideoCodec;

use crate::{
    IrohDestinationPeer, IrohNotifier, IrohSourcePeer,
    admission::{self, WELD_ALPN},
    peer::{spawn_destination_peer, spawn_source_peer},
    rendezvous,
};

const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);
const N0_ONLINE_WAIT: Duration = Duration::from_secs(30);

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;

/// Network services enabled for one Iroh endpoint.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IrohNetwork {
    /// Direct IP paths only. This mode has no DNS or relay dependency.
    #[default]
    Direct,
    /// N0 discovery, NAT traversal, and relay fallback.
    N0,
}

/// Long-lived Iroh endpoint host. Individual peers borrow its lifetime.
pub struct IrohHost {
    lifetime: Arc<HostLifetime>,
    ticket: String,
}

impl IrohHost {
    pub fn bind(network: IrohNetwork) -> Result<Self> {
        let (commands, receiver) = mpsc::unbounded_channel();
        let (started_tx, started_rx) = std_mpsc::sync_channel(1);
        let (done_tx, done_rx) = std_mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("weld-iroh".to_owned())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("could not create Iroh Tokio runtime")
                    .and_then(|runtime| runtime.block_on(run_host(network, receiver, started_tx)));
                if let Err(error) = result {
                    tracing::error!(%error, "Iroh host stopped");
                }
                let _ = done_tx.send(());
            })
            .context("could not spawn Iroh host thread")?;
        let ticket = started_rx
            .recv()
            .context("Iroh host stopped during startup")?
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            lifetime: Arc::new(HostLifetime {
                commands,
                worker: Mutex::new(Some(worker)),
                done: Mutex::new(done_rx),
                accepting: Arc::new(AtomicBool::new(false)),
            }),
            ticket,
        })
    }

    /// Publishes the source ticket, then admits only the destination named by a trusted file.
    /// Both files must be in private current-user directories; publications are one-shot.
    /// The timeout covers the file exchange and admission together, excluding endpoint bind.
    /// Only one acceptor may wait on this host at a time: incoming connections share
    /// its accept queue. Concurrent multi-peer admission needs a central dispatcher.
    pub fn accept_source(
        &self,
        ticket_path: impl AsRef<Path>,
        expected_peer_path: impl AsRef<Path>,
        codec: VideoCodec,
        notifier: IrohNotifier,
        startup_timeout: Duration,
    ) -> Result<IrohSourcePeer> {
        self.begin_accept_source(
            ticket_path,
            expected_peer_path,
            codec,
            notifier,
            startup_timeout,
        )?
        .wait()
    }

    /// Begins one cancellable admission without waiting for the approved identity
    /// or peer. Poll from the host loop when the supplied notifier becomes ready.
    pub fn begin_accept_source(
        &self,
        ticket_path: impl AsRef<Path>,
        expected_peer_path: impl AsRef<Path>,
        codec: VideoCodec,
        notifier: IrohNotifier,
        startup_timeout: Duration,
    ) -> Result<PendingSourceAdmission> {
        let deadline = Instant::now()
            .checked_add(startup_timeout)
            .context("Iroh startup timeout exceeds clock range")?;
        self.lifetime
            .accepting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("an Iroh source admission is already pending"))?;
        let guard = AcceptGuard(self.lifetime.accepting.clone());
        let expected = rendezvous::PublicationReader::new(expected_peer_path.as_ref())?;
        rendezvous::publish(ticket_path.as_ref(), &self.ticket)?;
        let (reply, result) = oneshot::channel();
        let (cancel, cancelled) = oneshot::channel();
        self.lifetime
            .commands
            .send(HostCommand::AcceptSource {
                host: Arc::downgrade(&self.lifetime),
                codec,
                expected,
                deadline,
                notifier,
                reply,
                cancelled,
                guard,
            })
            .map_err(|_| anyhow::anyhow!("Iroh host is unavailable"))?;
        tracing::info!(path = %ticket_path.as_ref().display(), "waiting for an Iroh hoist destination");
        Ok(PendingSourceAdmission {
            result: Some(result),
            cancel: Some(cancel),
            _host: self.lifetime.clone(),
        })
    }

    /// Publishes this process's ephemeral public identity before it waits for a source.
    pub fn publish_identity(&self, path: impl AsRef<Path>) -> Result<()> {
        let ticket =
            EndpointTicket::from_str(&self.ticket).context("local Iroh ticket is invalid")?;
        rendezvous::publish(path.as_ref(), &ticket.endpoint_addr().id.to_string())
    }

    /// Connects using a trusted source ticket. The timeout covers file exchange and bootstrap.
    pub fn connect_destination(
        &self,
        ticket_path: impl AsRef<Path>,
        supported_codecs: Vec<VideoCodec>,
        notifier: IrohNotifier,
        startup_timeout: Duration,
    ) -> Result<IrohDestinationPeer> {
        let deadline = Instant::now()
            .checked_add(startup_timeout)
            .context("Iroh startup timeout exceeds clock range")?;
        let encoded = rendezvous::read(ticket_path.as_ref(), deadline)?;
        let ticket =
            EndpointTicket::from_str(encoded.trim()).context("Iroh endpoint ticket is invalid")?;
        let (reply, result) = std_mpsc::sync_channel(1);
        self.lifetime
            .commands
            .send(HostCommand::ConnectDestination {
                host: Arc::downgrade(&self.lifetime),
                ticket,
                supported_codecs,
                deadline,
                notifier,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("Iroh host is unavailable"))?;
        result
            .recv()
            .context("Iroh host stopped while connecting a peer")?
            .map_err(anyhow::Error::msg)
    }

    pub fn ticket(&self) -> &str {
        &self.ticket
    }
}

pub(crate) struct HostLifetime {
    commands: mpsc::UnboundedSender<HostCommand>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    done: Mutex<std_mpsc::Receiver<()>>,
    accepting: Arc<AtomicBool>,
}

impl Drop for HostLifetime {
    fn drop(&mut self) {
        let _ = self.commands.send(HostCommand::Shutdown);
        let dropping_on_worker = self
            .worker
            .lock()
            .ok()
            .and_then(|worker| {
                worker
                    .as_ref()
                    .map(|worker| worker.thread().id() == thread::current().id())
            })
            .unwrap_or(false);
        if dropping_on_worker {
            let _ = self.worker.lock().ok().and_then(|mut worker| worker.take());
            return;
        }
        let completed = self
            .done
            .lock()
            .ok()
            .is_some_and(|done| done.recv_timeout(SHUTDOWN_WAIT).is_ok());
        let worker = self.worker.lock().ok().and_then(|mut worker| worker.take());
        if completed {
            if let Some(worker) = worker {
                let _ = worker.join();
            }
        } else {
            tracing::warn!("detached an Iroh host that did not stop within the shutdown budget");
        }
    }
}

/// A single admission result. Dropping an unfinished or unclaimed result cancels
/// it; an already-connected but unclaimed peer is explicitly disconnected.
pub struct PendingSourceAdmission {
    result: Option<oneshot::Receiver<Result<IrohSourcePeer, String>>>,
    cancel: Option<oneshot::Sender<()>>,
    _host: Arc<HostLifetime>,
}

impl PendingSourceAdmission {
    pub fn poll(&mut self) -> Result<Option<IrohSourcePeer>> {
        let result = self
            .result
            .as_mut()
            .context("Iroh admission result was already consumed")?;
        match result.try_recv() {
            Ok(result) => {
                self.result = None;
                self.cancel = None;
                result.map(Some).map_err(anyhow::Error::msg)
            }
            Err(oneshot::error::TryRecvError::Empty) => Ok(None),
            Err(oneshot::error::TryRecvError::Closed) => {
                self.result = None;
                Err(anyhow::anyhow!("Iroh host stopped during admission"))
            }
        }
    }
    fn wait(mut self) -> Result<IrohSourcePeer> {
        let result = self
            .result
            .take()
            .context("Iroh admission result was already consumed")?
            .blocking_recv()
            .context("Iroh host stopped during admission")?;
        self.cancel = None;
        result.map_err(anyhow::Error::msg)
    }
    pub fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(mut result) = self.result.take()
            && let Ok(Ok(peer)) = result.try_recv()
        {
            peer.disconnect();
        }
    }
}

impl Drop for PendingSourceAdmission {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct AcceptGuard(Arc<AtomicBool>);
impl Drop for AcceptGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

enum HostCommand {
    AcceptSource {
        host: Weak<HostLifetime>,
        codec: VideoCodec,
        expected: rendezvous::PublicationReader,
        deadline: Instant,
        notifier: IrohNotifier,
        reply: oneshot::Sender<Result<IrohSourcePeer, String>>,
        cancelled: oneshot::Receiver<()>,
        guard: AcceptGuard,
    },
    ConnectDestination {
        host: Weak<HostLifetime>,
        ticket: EndpointTicket,
        supported_codecs: Vec<VideoCodec>,
        deadline: Instant,
        notifier: IrohNotifier,
        reply: std_mpsc::SyncSender<Result<IrohDestinationPeer, String>>,
    },
    Shutdown,
}

async fn run_host(
    network: IrohNetwork,
    mut commands: mpsc::UnboundedReceiver<HostCommand>,
    started: std_mpsc::SyncSender<Result<String, String>>,
) -> Result<()> {
    let endpoint = match network {
        IrohNetwork::Direct => {
            Endpoint::builder(presets::Minimal)
                .relay_mode(RelayMode::Disabled)
                .alpns(vec![WELD_ALPN.to_vec()])
                .bind()
                .await
        }
        IrohNetwork::N0 => {
            Endpoint::builder(presets::N0)
                .alpns(vec![WELD_ALPN.to_vec()])
                .bind()
                .await
        }
    }
    .map_err(anyhow::Error::from)?;
    if network == IrohNetwork::N0 {
        tokio::time::timeout(N0_ONLINE_WAIT, endpoint.online())
            .await
            .context("Iroh N0 endpoint did not become online within 30 seconds")?;
    }
    let address = endpoint.watch_addr().get();
    if network == IrohNetwork::Direct && address.ip_addrs().next().is_none() {
        bail!("Iroh endpoint published no direct listening address");
    }
    let ticket = EndpointTicket::new(address).to_string();
    started
        .send(Ok(ticket))
        .map_err(|_| anyhow::anyhow!("Iroh host startup receiver disappeared"))?;

    while let Some(command) = commands.recv().await {
        match command {
            HostCommand::AcceptSource {
                host,
                codec,
                expected,
                deadline,
                notifier,
                reply,
                cancelled,
                guard,
            } => {
                let endpoint = endpoint.clone();
                tokio::spawn(async move {
                    let _guard = guard;
                    let admitted = async {
                        let expected = loop {
                            if let Some(value) = expected.try_read()? {
                                break EndpointId::from_str(value.trim())
                                    .context("approved Iroh peer identity is invalid")?;
                            }
                            let remaining = deadline.saturating_duration_since(Instant::now());
                            anyhow::ensure!(
                                !remaining.is_zero(),
                                "timed out waiting for approved Iroh identity"
                            );
                            tokio::time::sleep(Duration::from_millis(20).min(remaining)).await;
                        };
                        accept_source(host, endpoint, expected, codec, deadline, notifier.clone())
                            .await
                    };
                    let result = tokio::select! {
                        biased;
                        _ = cancelled => Err(anyhow::anyhow!("Iroh admission cancelled")),
                        result = admitted => result,
                    }
                    .map_err(|error| format!("{error:#}"));
                    if let Err(Ok(peer)) = reply.send(result) {
                        peer.disconnect();
                    }
                    if let Err(error) = notifier.notify() {
                        tracing::warn!(%error, "could not wake host after Iroh admission");
                    }
                });
            }
            HostCommand::ConnectDestination {
                host,
                ticket,
                supported_codecs,
                deadline,
                notifier,
                reply,
            } => {
                let endpoint = endpoint.clone();
                tokio::spawn(async move {
                    let result = connect_destination(
                        host,
                        endpoint,
                        ticket,
                        supported_codecs,
                        deadline,
                        notifier,
                    )
                    .await
                    .map_err(|error| format!("{error:#}"));
                    let _ = reply.send(result);
                });
            }
            HostCommand::Shutdown => break,
        }
    }
    endpoint.close().await;
    Ok(())
}

async fn accept_source(
    host: Weak<HostLifetime>,
    endpoint: Endpoint,
    expected: EndpointId,
    codec: VideoCodec,
    deadline: Instant,
    notifier: IrohNotifier,
) -> Result<IrohSourcePeer> {
    let mut bootstrap = admission::accept_source(
        &endpoint,
        expected,
        codec,
        deadline.into(),
        admission::ATTEMPT_TIMEOUT,
    )
    .await?;
    let host = host
        .upgrade()
        .context("Iroh host was dropped during peer setup")?;
    let peer = spawn_source_peer(
        host,
        bootstrap.pending.connection.clone(),
        bootstrap.send,
        bootstrap.recv,
        bootstrap.media,
        notifier,
        codec,
    );
    bootstrap.pending.hand_off();
    Ok(peer)
}

async fn connect_destination(
    host: Weak<HostLifetime>,
    endpoint: Endpoint,
    ticket: EndpointTicket,
    supported_codecs: Vec<VideoCodec>,
    deadline: Instant,
    notifier: IrohNotifier,
) -> Result<IrohDestinationPeer> {
    let mut bootstrap =
        admission::connect_destination(&endpoint, ticket, &supported_codecs, deadline.into())
            .await?;
    let host = host
        .upgrade()
        .context("Iroh host was dropped during peer setup")?;
    let peer = spawn_destination_peer(
        host,
        bootstrap.pending.connection.clone(),
        bootstrap.send,
        bootstrap.recv,
        bootstrap.media,
        notifier,
        bootstrap.codec,
    );
    bootstrap.pending.hand_off();
    Ok(peer)
}
