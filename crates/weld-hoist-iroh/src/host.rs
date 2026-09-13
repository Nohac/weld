//! One long-lived Iroh endpoint with disposable peer connections.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
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
use iroh::{
    Endpoint, EndpointAddr, RelayMode, SecretKey, Watcher,
    dns::{DnsProtocol, DnsResolver},
    endpoint::presets,
};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::sync::{mpsc, oneshot};
use weld_hoist_encoded::{EncodedDestinationTransport, EncodedSourceTransport};
use weld_media::VideoCodec;

use crate::{
    IrohConnectionProfile, IrohDestinationPeer, IrohDeviceIdentity, IrohNotifier, IrohPeerIdentity,
    IrohSourcePeer, IrohTrustedPeers,
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

/// DNS policy for N0 discovery/relay names. Direct mode sends no DNS queries.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IrohDnsPolicy {
    /// Iroh's system-aware resolver, including its upstream fallback policy.
    #[default]
    System,
    /// Development policy: query public resolvers without reading system DNS.
    /// Uses Google IPv4/IPv6 over UDP/TCP plus any upstream public fallbacks.
    /// Does not respect Android Private DNS or a VPN's resolver configuration.
    Public,
}

impl IrohDnsPolicy {
    // The compositor locks iroh-dns 1.1; the standalone shell resolves 1.3.
    // The replacement API is absent in 1.1. `expect` would warn there because
    // no deprecation is emitted, so narrowly allow this compatibility call.
    #[allow(
        deprecated,
        reason = "nameserver API shared by the two locked Iroh DNS versions"
    )]
    fn resolver(self) -> Option<DnsResolver> {
        match self {
            Self::System => None,
            Self::Public => {
                // Do not call with_system_defaults: Android's JNI context is not
                // installed by GDExtension. Upstream otherwise detects that via
                // a panic, which becomes fatal with panic=abort.
                let addresses = [
                    IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
                    IpAddr::V4(Ipv4Addr::new(8, 8, 4, 4)),
                    IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888)),
                    IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8844)),
                ];
                let mut resolver = DnsResolver::builder();
                for address in addresses {
                    resolver = resolver
                        .with_nameserver(SocketAddr::new(address, 53), DnsProtocol::Udp)
                        .with_nameserver(SocketAddr::new(address, 53), DnsProtocol::Tcp);
                }
                Some(resolver.build())
            }
        }
    }
}

/// Long-lived Iroh endpoint host. Individual peers borrow its lifetime.
pub struct IrohHost {
    lifetime: Arc<HostLifetime>,
    ticket: String,
    network: IrohNetwork,
}

impl IrohHost {
    pub fn bind(network: IrohNetwork) -> Result<Self> {
        Self::bind_key(network, None, IrohDnsPolicy::System)
    }

    /// Binds with a persisted local key. N0 publishes a stable, linkable endpoint
    /// identity; it does not make every discovered device an authorized peer.
    pub fn bind_with_identity(network: IrohNetwork, identity: &IrohDeviceIdentity) -> Result<Self> {
        Self::bind_with_identity_and_dns(network, identity, IrohDnsPolicy::System)
    }

    /// Binds a stable identity with an explicit, process-local DNS policy.
    /// The policy is not persisted in or accepted from a peer's public profile.
    pub fn bind_with_identity_and_dns(
        network: IrohNetwork,
        identity: &IrohDeviceIdentity,
        dns: IrohDnsPolicy,
    ) -> Result<Self> {
        Self::bind_key(network, Some(identity.secret()), dns)
    }

    fn bind_key(
        network: IrohNetwork,
        secret: Option<SecretKey>,
        dns: IrohDnsPolicy,
    ) -> Result<Self> {
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
                    .and_then(|runtime| {
                        runtime.block_on(run_host(network, secret, dns, receiver, started_tx))
                    });
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
            network,
        })
    }

    /// Publishes the source ticket, then admits only the destination named by a trusted file.
    /// Both files must be in private current-user directories; publications are one-shot.
    /// The timeout covers the file exchange and admission together, excluding endpoint bind.
    /// Only one acceptor may wait on this host at a time: incoming connections share
    /// its accept queue. Concurrent multi-peer admission needs a central dispatcher.
    /// Call only outside an async runtime; use [`Self::begin_accept_source`] there.
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
        self.begin_admission(
            SourceApproval::Publication(expected),
            codec,
            notifier,
            deadline,
            guard,
        )
    }

    /// Admit one of the explicitly trusted devices without a new file exchange.
    /// Only one pending acceptor is allowed; callers own active-viewer policy.
    pub fn begin_accept_trusted_source(
        &self,
        trusted: IrohTrustedPeers,
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
        self.begin_admission(
            SourceApproval::Trusted(trusted),
            codec,
            notifier,
            deadline,
            AcceptGuard(self.lifetime.accepting.clone()),
        )
    }

    fn begin_admission(
        &self,
        expected: SourceApproval,
        codec: VideoCodec,
        notifier: IrohNotifier,
        deadline: Instant,
        guard: AcceptGuard,
    ) -> Result<PendingSourceAdmission> {
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
        tracing::info!("waiting for an approved Iroh hoist destination");
        Ok(PendingSourceAdmission {
            result: Some(result),
            cancel: Some(cancel),
            _host: self.lifetime.clone(),
        })
    }

    /// Publishes this endpoint's public identity before it waits for a source.
    pub fn publish_identity(&self, path: impl AsRef<Path>) -> Result<()> {
        let ticket =
            EndpointTicket::from_str(&self.ticket).context("local Iroh ticket is invalid")?;
        rendezvous::publish(path.as_ref(), &ticket.endpoint_addr().id.to_string())
    }

    /// Connects using a trusted source ticket. The timeout covers file exchange and bootstrap.
    /// Call only outside an async runtime; use [`Self::begin_connect_profile`] there.
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
        self.begin_connect_address(
            ticket.endpoint_addr().clone(),
            supported_codecs,
            notifier,
            deadline,
        )?
        .wait()
    }

    /// Nonblocking dial of a saved, pinned source. The host's network preset
    /// must match the profile; enabling public discovery is never implicit.
    pub fn begin_connect_profile(
        &self,
        profile: &IrohConnectionProfile,
        supported_codecs: Vec<VideoCodec>,
        notifier: IrohNotifier,
        startup_timeout: Duration,
    ) -> Result<PendingDestinationConnection> {
        anyhow::ensure!(
            self.network == profile.network(),
            "Iroh profile network differs from bound host"
        );
        let deadline = Instant::now()
            .checked_add(startup_timeout)
            .context("Iroh startup timeout exceeds clock range")?;
        self.begin_connect_address(
            profile.endpoint_addr()?,
            supported_codecs,
            notifier,
            deadline,
        )
    }

    fn begin_connect_address(
        &self,
        address: EndpointAddr,
        supported_codecs: Vec<VideoCodec>,
        notifier: IrohNotifier,
        deadline: Instant,
    ) -> Result<PendingDestinationConnection> {
        let (reply, result) = oneshot::channel();
        let (cancel, cancelled) = oneshot::channel();
        self.lifetime
            .commands
            .send(HostCommand::ConnectDestination {
                host: Arc::downgrade(&self.lifetime),
                address,
                supported_codecs,
                deadline,
                notifier,
                reply,
                cancelled,
            })
            .map_err(|_| anyhow::anyhow!("Iroh host is unavailable"))?;
        Ok(PendingDestinationConnection {
            result: Some(result),
            cancel: Some(cancel),
            _host: self.lifetime.clone(),
        })
    }

    pub fn ticket(&self) -> &str {
        &self.ticket
    }

    /// Snapshot this endpoint's public identity and dialing hints. Direct hints
    /// expire on rebind; N0 profiles can discover the same identity after restart.
    pub fn connection_profile(&self) -> Result<IrohConnectionProfile> {
        let ticket = EndpointTicket::from_str(&self.ticket).context("invalid local ticket")?;
        let address = ticket.endpoint_addr();
        IrohConnectionProfile::new(
            IrohPeerIdentity(address.id.to_string()),
            self.network,
            address.ip_addrs().copied().collect(),
        )
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

/// One cancellable outgoing connection. Drop closes even a connected result
/// that has not yet been claimed; polling never blocks the application's thread.
pub struct PendingDestinationConnection {
    result: Option<oneshot::Receiver<Result<IrohDestinationPeer, String>>>,
    cancel: Option<oneshot::Sender<()>>,
    _host: Arc<HostLifetime>,
}

impl PendingDestinationConnection {
    pub fn poll(&mut self) -> Result<Option<IrohDestinationPeer>> {
        let result = self
            .result
            .as_mut()
            .context("Iroh connection result was already consumed")?;
        match result.try_recv() {
            Ok(result) => {
                self.result = None;
                self.cancel = None;
                result.map(Some).map_err(anyhow::Error::msg)
            }
            Err(oneshot::error::TryRecvError::Empty) => Ok(None),
            Err(oneshot::error::TryRecvError::Closed) => {
                self.result = None;
                Err(anyhow::anyhow!("Iroh host stopped during connection"))
            }
        }
    }
    fn wait(mut self) -> Result<IrohDestinationPeer> {
        let result = self
            .result
            .take()
            .context("Iroh connection result was already consumed")?
            .blocking_recv()
            .context("Iroh host stopped during connection")?;
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
impl Drop for PendingDestinationConnection {
    fn drop(&mut self) {
        self.cancel();
    }
}

enum SourceApproval {
    Publication(rendezvous::PublicationReader),
    Trusted(IrohTrustedPeers),
}
impl SourceApproval {
    async fn resolve(self, deadline: Instant) -> Result<IrohTrustedPeers> {
        let reader = match self {
            Self::Publication(reader) => reader,
            Self::Trusted(peers) => return Ok(peers),
        };
        loop {
            if let Some(value) = reader.try_read()? {
                let peer = IrohPeerIdentity::from_str(value.trim())
                    .context("approved Iroh peer identity is invalid")?;
                return IrohTrustedPeers::new(vec![peer]);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            anyhow::ensure!(
                !remaining.is_zero(),
                "timed out waiting for approved Iroh identity"
            );
            tokio::time::sleep(Duration::from_millis(20).min(remaining)).await;
        }
    }
}

enum HostCommand {
    AcceptSource {
        host: Weak<HostLifetime>,
        codec: VideoCodec,
        expected: SourceApproval,
        deadline: Instant,
        notifier: IrohNotifier,
        reply: oneshot::Sender<Result<IrohSourcePeer, String>>,
        cancelled: oneshot::Receiver<()>,
        guard: AcceptGuard,
    },
    ConnectDestination {
        host: Weak<HostLifetime>,
        address: EndpointAddr,
        supported_codecs: Vec<VideoCodec>,
        deadline: Instant,
        notifier: IrohNotifier,
        reply: oneshot::Sender<Result<IrohDestinationPeer, String>>,
        cancelled: oneshot::Receiver<()>,
    },
    Shutdown,
}

async fn run_host(
    network: IrohNetwork,
    secret: Option<SecretKey>,
    dns: IrohDnsPolicy,
    mut commands: mpsc::UnboundedReceiver<HostCommand>,
    started: std_mpsc::SyncSender<Result<String, String>>,
) -> Result<()> {
    let secret = secret.unwrap_or_else(SecretKey::generate);
    let mut endpoint = match network {
        IrohNetwork::Direct => Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled),
        IrohNetwork::N0 => Endpoint::builder(presets::N0),
    };
    if let Some(resolver) = dns.resolver() {
        endpoint = endpoint.dns_resolver(resolver);
    }
    let endpoint = endpoint
        .secret_key(secret)
        .alpns(vec![WELD_ALPN.to_vec()])
        .bind()
        .await
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
                        let expected = expected.resolve(deadline).await?;
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
                address,
                supported_codecs,
                deadline,
                notifier,
                reply,
                cancelled,
            } => {
                let endpoint = endpoint.clone();
                tokio::spawn(async move {
                    let connecting = connect_destination(
                        host,
                        endpoint,
                        address,
                        supported_codecs,
                        deadline,
                        notifier.clone(),
                    );
                    let result = tokio::select! {
                        biased;
                        _ = cancelled => Err(anyhow::anyhow!("Iroh connection cancelled")),
                        result = connecting => result,
                    }
                    .map_err(|error| format!("{error:#}"));
                    if let Err(Ok(peer)) = reply.send(result) {
                        peer.disconnect();
                    }
                    if let Err(error) = notifier.notify() {
                        tracing::warn!(%error, "could not wake host after Iroh connection");
                    }
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
    expected: IrohTrustedPeers,
    codec: VideoCodec,
    deadline: Instant,
    notifier: IrohNotifier,
) -> Result<IrohSourcePeer> {
    let mut bootstrap = admission::accept_trusted_source(
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
    address: EndpointAddr,
    supported_codecs: Vec<VideoCodec>,
    deadline: Instant,
    notifier: IrohNotifier,
) -> Result<IrohDestinationPeer> {
    let mut bootstrap =
        admission::connect_address(&endpoint, address, &supported_codecs, deadline.into()).await?;
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
