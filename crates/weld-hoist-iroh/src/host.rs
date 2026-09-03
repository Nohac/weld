//! One long-lived Iroh endpoint with disposable peer connections.

use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    str::FromStr,
    sync::{Arc, Mutex, Weak, mpsc as std_mpsc},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use iroh::{Endpoint, RelayMode, Watcher, endpoint::presets};
use iroh_tickets::endpoint::EndpointTicket;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use weld_core::host::ClientRuntimeNotifier;
use weld_hoist_protocol::{ProtocolRevision, SurfaceMode};
use weld_media::VideoCodec;

use crate::{
    IrohDestinationPeer, IrohSourcePeer,
    framing::{read_record, write_record},
    peer::{spawn_destination_peer, spawn_source_peer},
};

const WELD_ALPN: &[u8] = b"weld/hoist/1";
const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);
const N0_ONLINE_WAIT: Duration = Duration::from_secs(30);

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
            }),
            ticket,
        })
    }

    /// Atomically publishes this host's current endpoint ticket and accepts one source peer.
    pub fn accept_source(
        &self,
        ticket_path: impl AsRef<Path>,
        codec: VideoCodec,
        notifier: ClientRuntimeNotifier,
    ) -> Result<IrohSourcePeer> {
        publish_ticket(ticket_path.as_ref(), &self.ticket)?;
        tracing::info!(path = %ticket_path.as_ref().display(), "waiting for an Iroh hoist destination");
        let (reply, result) = std_mpsc::sync_channel(1);
        self.lifetime
            .commands
            .send(HostCommand::AcceptSource {
                host: Arc::downgrade(&self.lifetime),
                codec,
                notifier,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("Iroh host is unavailable"))?;
        result
            .recv()
            .context("Iroh host stopped while accepting a peer")?
            .map_err(anyhow::Error::msg)
    }

    /// Connects one destination peer using a published endpoint ticket.
    pub fn connect_destination(
        &self,
        ticket_path: impl AsRef<Path>,
        supported_codecs: Vec<VideoCodec>,
        notifier: ClientRuntimeNotifier,
    ) -> Result<IrohDestinationPeer> {
        let encoded = std::fs::read_to_string(ticket_path.as_ref()).with_context(|| {
            format!(
                "could not read Iroh endpoint ticket {}",
                ticket_path.as_ref().display()
            )
        })?;
        let ticket =
            EndpointTicket::from_str(encoded.trim()).context("Iroh endpoint ticket is invalid")?;
        let (reply, result) = std_mpsc::sync_channel(1);
        self.lifetime
            .commands
            .send(HostCommand::ConnectDestination {
                host: Arc::downgrade(&self.lifetime),
                ticket,
                supported_codecs,
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

enum HostCommand {
    AcceptSource {
        host: Weak<HostLifetime>,
        codec: VideoCodec,
        notifier: ClientRuntimeNotifier,
        reply: std_mpsc::SyncSender<Result<IrohSourcePeer, String>>,
    },
    ConnectDestination {
        host: Weak<HostLifetime>,
        ticket: EndpointTicket,
        supported_codecs: Vec<VideoCodec>,
        notifier: ClientRuntimeNotifier,
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
                notifier,
                reply,
            } => {
                let endpoint = endpoint.clone();
                tokio::spawn(async move {
                    let result = accept_source(host, endpoint, codec, notifier)
                        .await
                        .map_err(|error| error.to_string());
                    let _ = reply.send(result);
                });
            }
            HostCommand::ConnectDestination {
                host,
                ticket,
                supported_codecs,
                notifier,
                reply,
            } => {
                let endpoint = endpoint.clone();
                tokio::spawn(async move {
                    let result =
                        connect_destination(host, endpoint, ticket, supported_codecs, notifier)
                            .await
                            .map_err(|error| error.to_string());
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
    codec: VideoCodec,
    notifier: ClientRuntimeNotifier,
) -> Result<IrohSourcePeer> {
    let incoming = endpoint
        .accept()
        .await
        .context("Iroh endpoint closed before accepting a peer")?;
    let connection = incoming.await.context("could not accept Iroh peer")?;
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("could not open Iroh control stream")?;
    write_record(
        &mut send,
        &BootstrapOffer {
            revision: ProtocolRevision::CURRENT,
            role: PeerRole::Source,
            mode: SurfaceMode::EncodedOpaque(codec),
        },
    )
    .await?;
    let answer: BootstrapAnswer = read_record(&mut recv).await?;
    ProtocolRevision::CURRENT.ensure_compatible(answer.revision)?;
    if answer.role != PeerRole::Destination {
        bail!("Iroh bootstrap peer returned the wrong role");
    }
    if let Some(rejection) = answer.rejection {
        bail!("Iroh destination rejected the session: {rejection}");
    }
    let media = connection
        .open_uni()
        .await
        .context("could not open Iroh media stream")?;
    let host = host
        .upgrade()
        .context("Iroh host was dropped during peer setup")?;
    Ok(spawn_source_peer(
        host, connection, send, recv, media, notifier, codec,
    ))
}

async fn connect_destination(
    host: Weak<HostLifetime>,
    endpoint: Endpoint,
    ticket: EndpointTicket,
    supported_codecs: Vec<VideoCodec>,
    notifier: ClientRuntimeNotifier,
) -> Result<IrohDestinationPeer> {
    let connection = endpoint
        .connect(ticket.endpoint_addr().clone(), WELD_ALPN)
        .await
        .context("could not connect to Iroh source")?;
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .context("could not accept Iroh control stream")?;
    let offer: BootstrapOffer = read_record(&mut recv).await?;
    let codec = match offer.mode {
        SurfaceMode::EncodedOpaque(codec) => Some(codec),
        SurfaceMode::Native => None,
    };
    let rejection = validate_offer(&offer, &supported_codecs)
        .err()
        .map(|error| error.to_string());
    write_record(
        &mut send,
        &BootstrapAnswer {
            revision: ProtocolRevision::CURRENT,
            role: PeerRole::Destination,
            rejection: rejection.clone(),
        },
    )
    .await?;
    if let Some(rejection) = rejection {
        bail!("rejected Iroh source: {rejection}");
    }
    let codec = codec.context("accepted Iroh offer did not select an encoded codec")?;
    let media = connection
        .accept_uni()
        .await
        .context("could not accept Iroh media stream")?;
    let host = host
        .upgrade()
        .context("Iroh host was dropped during peer setup")?;
    Ok(spawn_destination_peer(
        host, connection, send, recv, media, notifier, codec,
    ))
}

fn validate_offer(offer: &BootstrapOffer, supported_codecs: &[VideoCodec]) -> Result<()> {
    ProtocolRevision::CURRENT.ensure_compatible(offer.revision)?;
    if offer.role != PeerRole::Source {
        bail!("Iroh bootstrap peer has the wrong role");
    }
    let SurfaceMode::EncodedOpaque(codec) = offer.mode else {
        bail!("Iroh source selected an unsupported surface mode");
    };
    if !supported_codecs.contains(&codec) {
        bail!("Iroh source selected an unsupported codec");
    }
    Ok(())
}

fn publish_ticket(path: &Path, ticket: &str) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("Iroh ticket path has no UTF-8 file name")?;
    let temporary = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .with_context(|| format!("could not create Iroh ticket {}", temporary.display()))?;
    let result = (|| {
        file.write_all(ticket.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok::<_, std::io::Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.with_context(|| format!("could not publish Iroh ticket {}", path.display()))
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum PeerRole {
    Source,
    Destination,
}

#[derive(Debug, Deserialize, Serialize)]
struct BootstrapOffer {
    revision: ProtocolRevision,
    role: PeerRole,
    mode: SurfaceMode,
}

#[derive(Debug, Deserialize, Serialize)]
struct BootstrapAnswer {
    revision: ProtocolRevision,
    role: PeerRole,
    rejection: Option<String>,
}
