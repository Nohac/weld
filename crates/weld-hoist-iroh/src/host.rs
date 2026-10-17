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
    endpoint::{PortmapperConfig, presets},
};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::sync::mpsc;
use weld_hoist_core::session::server::SessionRuntime;
use weld_hoist_encoded::{EncodedDestinationTransport, EncodedSourceTransport};
use weld_media::VideoCodec;

use crate::{
    IrohConnectionProfile, IrohDestinationPeer, IrohDeviceIdentity, IrohNotifier, IrohPeerIdentity,
    IrohSourcePeer, IrohTrustedPeers, adb, admission, rendezvous,
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
    /// Explicit launcher-managed loopback TCP tunnel, with no IP/relay fallback.
    Adb,
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
#[derive(Clone)]
pub struct IrohHost {
    sessions: SessionRuntime,
    transport: Arc<TransportResources>,
    lifetime: Arc<HostLifetime>,
    ticket: String,
    network: IrohNetwork,
    adb_listen: Option<SocketAddr>,
    pairing: crate::pairing::PairingHost,
}

impl IrohHost {
    pub fn bind(sessions: &SessionRuntime, network: IrohNetwork) -> Result<Self> {
        Self::bind_key(sessions, network, None, IrohDnsPolicy::System, None, None)
    }

    /// Bind exactly the application's service registry. Hoist/pairing endpoints
    /// are available when their handlers have been registered by the assembly.
    pub fn bind_registered(
        sessions: &SessionRuntime,
        network: IrohNetwork,
        services: weld_session::Registry<crate::IrohConnection>,
    ) -> Result<Self> {
        Self::bind_key(
            sessions,
            network,
            None,
            IrohDnsPolicy::System,
            None,
            Some(services),
        )
    }

    /// Binds with a persisted local key. N0 publishes a stable, linkable endpoint
    /// identity; it does not make every discovered device an authorized peer.
    pub fn bind_with_identity(
        sessions: &SessionRuntime,
        network: IrohNetwork,
        identity: &IrohDeviceIdentity,
    ) -> Result<Self> {
        Self::bind_with_identity_and_dns(sessions, network, identity, IrohDnsPolicy::System)
    }

    /// Binds a stable identity with an explicit, process-local DNS policy.
    /// The policy is not persisted in or accepted from a peer's public profile.
    pub fn bind_with_identity_and_dns(
        sessions: &SessionRuntime,
        network: IrohNetwork,
        identity: &IrohDeviceIdentity,
        dns: IrohDnsPolicy,
    ) -> Result<Self> {
        Self::bind_key(sessions, network, Some(identity.secret()), dns, None, None)
    }

    /// Bind a custom-only source and resolve its loopback TCP listener before
    /// publishing a profile. ADB itself remains owned by the caller's launcher.
    /// Port zero selects a free host port; addresses never authorize viewers.
    pub fn bind_adb_source(
        sessions: &SessionRuntime,
        identity: &IrohDeviceIdentity,
        listen: SocketAddr,
    ) -> Result<Self> {
        anyhow::ensure!(
            listen.ip().is_loopback(),
            "ADB listener must be loopback TCP"
        );
        Self::bind_key(
            sessions,
            IrohNetwork::Adb,
            Some(identity.secret()),
            IrohDnsPolicy::System,
            Some(listen),
            None,
        )
    }

    fn bind_key(
        sessions: &SessionRuntime,
        network: IrohNetwork,
        secret: Option<SecretKey>,
        dns: IrohDnsPolicy,
        adb_listen: Option<SocketAddr>,
        services: Option<weld_session::Registry<crate::IrohConnection>>,
    ) -> Result<Self> {
        let (commands, receiver) = mpsc::unbounded_channel();
        let (started_tx, started_rx) = std_mpsc::sync_channel(1);
        let (done_tx, done_rx) = std_mpsc::sync_channel(1);
        let pairing = crate::pairing::PairingHost::default();
        let pair_worker = pairing.clone();
        let owner = Arc::new(Mutex::new(Weak::new()));
        let worker_owner = owner.clone();
        let session_runtime = sessions.clone();
        let worker = thread::Builder::new()
            .name("weld-iroh".to_owned())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("could not create Iroh Tokio runtime")
                    .and_then(|runtime| {
                        runtime.block_on(run_host(
                            session_runtime,
                            EndpointStartup {
                                network,
                                secret,
                                dns,
                                adb_listen,
                                services,
                            },
                            receiver,
                            started_tx,
                            pair_worker,
                            worker_owner,
                        ))
                    });
                if let Err(error) = result {
                    tracing::error!(%error, "Iroh host stopped");
                }
                let _ = done_tx.send(());
            })
            .context("could not spawn Iroh host thread")?;
        let BoundEndpoint {
            ticket,
            adb_listen,
            transport,
        } = started_rx
            .recv()
            .context("Iroh host stopped during startup")?
            .map_err(anyhow::Error::msg)?;
        let lifetime = Arc::new(HostLifetime {
            sessions: sessions.tasks(),
            commands,
            worker: Mutex::new(Some(worker)),
            done: Mutex::new(done_rx),
            accepting: Arc::new(AtomicBool::new(false)),
            reports: crate::DiagnosticReports::default(),
            diagnostic_permission: pairing.diagnostic_permission(),
            collecting: Arc::new(AtomicBool::new(false)),
        });
        *owner
            .lock()
            .map_err(|_| anyhow::anyhow!("endpoint owner poisoned"))? = Arc::downgrade(&lifetime);
        Ok(Self {
            sessions: sessions.clone(),
            transport: Arc::new(transport),
            lifetime,
            ticket,
            network,
            adb_listen,
            pairing,
        })
    }

    pub fn pairing(&self) -> crate::pairing::PairingHost {
        self.pairing.clone()
    }

    pub fn diagnostics(&self) -> crate::DiagnosticReports {
        self.lifetime.reports.clone()
    }

    /// Collect matching evidence over an existing connection to the same peer.
    /// Call from a control worker, outside an async runtime or compositor loop.
    pub fn collect_diagnostics(
        &self,
        session: weld_diagnostics::SessionId,
    ) -> Result<weld_diagnostics::collection::DiagnosticCollection> {
        let bundle = self
            .lifetime
            .reports
            .get(session)
            .context("diagnostic session unavailable or evicted")?;
        if self
            .lifetime
            .collecting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(weld_diagnostics::collection::DiagnosticCollection {
                bundle,
                status: weld_diagnostics::collection::CollectionStatus::Busy,
            });
        }
        let guard = CollectionGuard(self.lifetime.collecting.clone());
        let reports = self.lifetime.reports.clone();
        self.sessions
            .start_operation(
                "weld/diagnostic-collection/1",
                8,
                Arc::new(|| {}),
                |_| {},
                move |completion| async move {
                    let _guard = guard;
                    completion.send(reports.collect(session).await?)
                },
            )?
            .wait()
    }

    pub fn begin_device_session(
        &self,
        profile: IrohConnectionProfile,
        codecs: impl Into<weld_hoist_protocol::presentation::ReceiverPreferences>,
        notifier: IrohNotifier,
    ) -> Result<crate::pairing::PendingDeviceSession> {
        anyhow::ensure!(
            profile.network() == self.network,
            "device network differs from endpoint"
        );
        let wake = Arc::new(move || {
            let _ = notifier.notify();
        });
        weld_hoist_core::session::client::connect(
            &self.sessions,
            self.dialer(profile.endpoint_addr()?),
            codecs.into(),
            wake,
        )
    }

    pub fn begin_pairing(
        &self,
        invitation: crate::pairing::PairingInvitation,
        name: String,
        notifier: IrohNotifier,
    ) -> Result<crate::pairing::PendingPairing> {
        anyhow::ensure!(
            invitation.profile()?.network() == self.network,
            "pairing network differs from endpoint"
        );
        let connector = self.dialer(invitation.profile()?.endpoint_addr()?);
        let wake = Arc::new(move || {
            let _ = notifier.notify();
        });
        weld_hoist_core::session::client::pair(
            &self.sessions,
            connector,
            invitation.enrollment(name),
            wake,
        )
    }

    fn dialer(&self, address: EndpointAddr) -> crate::IrohConnector {
        crate::IrohConnector {
            endpoint: self.transport.endpoint.clone(),
            address,
            host: Arc::downgrade(&self.lifetime),
            io: self.transport.io.clone(),
            adb: None,
        }
    }

    /// Prepare a pinned peer connection for an application-selected service.
    pub fn connector(&self, profile: &IrohConnectionProfile) -> Result<crate::IrohConnector> {
        anyhow::ensure!(
            profile.network() == self.network,
            "profile network differs from endpoint"
        );
        let mut connector = self.dialer(profile.endpoint_addr()?);
        if self.network == IrohNetwork::Adb {
            connector.adb = Some((
                self.transport
                    .links
                    .clone()
                    .context("ADB manager missing")?,
                *profile
                    .addresses()
                    .first()
                    .context("ADB profile has no TCP address")?,
            ));
        }
        Ok(connector)
    }

    /// Admits only the destination named by a trusted file. Direct/N0 also
    /// publish a dialing ticket; ADB requires a separately published profile.
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
        anyhow::ensure!(
            self.network != IrohNetwork::Adb || self.adb_listen.is_some(),
            "ADB dial-only host cannot accept a source session"
        );
        let deadline = Instant::now()
            .checked_add(startup_timeout)
            .context("Iroh startup timeout exceeds clock range")?;
        self.lifetime
            .accepting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("an Iroh source admission is already pending"))?;
        let guard = AcceptGuard(self.lifetime.accepting.clone());
        let expected = rendezvous::PublicationReader::new(expected_peer_path.as_ref())?;
        // ADB profiles carry namespace-local TCP addresses; there is no portable
        // dialing ticket. Keep the identity-only ticket internal in this mode.
        if self.network != IrohNetwork::Adb {
            rendezvous::publish(ticket_path.as_ref(), &self.ticket)?;
        }
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
        anyhow::ensure!(
            self.network != IrohNetwork::Adb || self.adb_listen.is_some(),
            "ADB dial-only host cannot accept a source session"
        );
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
        let transport = self.transport.clone();
        let wake = notifier.clone();
        let operation = self.sessions.start_operation(
            "weld/direct-source/1",
            8,
            Arc::new(move || {
                let _ = wake.notify();
            }),
            |peer: &IrohSourcePeer| peer.disconnect(),
            move |completion| async move {
                let _guard = guard;
                let expected = expected.resolve(deadline).await?.identities()?;
                let incoming = crate::connection::DirectListener {
                    incoming: transport.incoming.clone(),
                    links: transport.links.clone(),
                };
                let peer = weld_hoist_core::session::bootstrap::accept(
                    incoming,
                    expected,
                    weld_hoist_protocol::SurfaceMode::EncodedOpaque(codec),
                    deadline,
                    Arc::new(move || {
                        let _ = notifier.notify();
                    }),
                )
                .await?;
                completion.send(peer)
            },
        )?;
        Ok(PendingSourceAdmission {
            operation,
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
        supported_codecs: impl Into<weld_hoist_protocol::presentation::ReceiverPreferences>,
        notifier: IrohNotifier,
        startup_timeout: Duration,
    ) -> Result<IrohDestinationPeer> {
        anyhow::ensure!(
            self.network != IrohNetwork::Adb,
            "ADB destinations require a connection profile, not a ticket"
        );
        let deadline = Instant::now()
            .checked_add(startup_timeout)
            .context("Iroh startup timeout exceeds clock range")?;
        let encoded = rendezvous::read(ticket_path.as_ref(), deadline)?;
        let ticket =
            EndpointTicket::from_str(encoded.trim()).context("Iroh endpoint ticket is invalid")?;
        self.begin_connect_address(
            ConnectTarget::Address(ticket.endpoint_addr().clone()),
            supported_codecs.into(),
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
        supported_codecs: impl Into<weld_hoist_protocol::presentation::ReceiverPreferences>,
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
            if self.network == IrohNetwork::Adb {
                ConnectTarget::Adb {
                    peer: profile.endpoint_addr()?.id,
                    address: *profile
                        .addresses()
                        .first()
                        .context("ADB profile has no TCP address")?,
                }
            } else {
                ConnectTarget::Address(profile.endpoint_addr()?)
            },
            supported_codecs.into(),
            notifier,
            deadline,
        )
    }

    fn begin_connect_address(
        &self,
        address: ConnectTarget,
        supported_codecs: weld_hoist_protocol::presentation::ReceiverPreferences,
        notifier: IrohNotifier,
        deadline: Instant,
    ) -> Result<PendingDestinationConnection> {
        let dialer = match address {
            ConnectTarget::Address(address) => self.dialer(address),
            ConnectTarget::Adb { peer, address } => {
                let mut dialer = self.dialer(EndpointAddr::new(peer));
                dialer.adb = Some((
                    self.transport
                        .links
                        .clone()
                        .context("ADB manager missing")?,
                    address,
                ));
                dialer
            }
        };
        let wake = notifier.clone();
        let operation = self.sessions.start_operation(
            "weld/direct-destination/1",
            8,
            Arc::new(move || {
                let _ = wake.notify();
            }),
            |peer: &IrohDestinationPeer| peer.disconnect(),
            move |completion| async move {
                let delivery = weld_hoist_core::session::bootstrap::Delivery {
                    native_buffers: false,
                    encoded: supported_codecs,
                };
                let peer = weld_hoist_core::session::bootstrap::connect(
                    crate::connection::DirectDialer(dialer),
                    delivery,
                    deadline,
                    Arc::new(move || {
                        let _ = notifier.notify();
                    }),
                )
                .await?;
                completion.send(peer)
            },
        )?;
        Ok(PendingDestinationConnection {
            operation,
            _host: self.lifetime.clone(),
        })
    }

    /// Endpoint ticket. In ADB mode this contains identity only, not a dialable
    /// route; use [`Self::connection_profile`] for the namespace-local TCP target.
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
            if self.network == IrohNetwork::Adb {
                vec![
                    self.adb_listen
                        .context("dial-only ADB host has no source profile")?,
                ]
            } else {
                address.ip_addrs().copied().collect()
            },
        )
    }
}

struct TransportResources {
    endpoint: Endpoint,
    io: tokio::runtime::Handle,
    incoming: admission::IncomingQueue,
    links: Option<Arc<adb::Manager>>,
}

struct BoundEndpoint {
    ticket: String,
    adb_listen: Option<SocketAddr>,
    transport: TransportResources,
}

pub(crate) struct HostLifetime {
    pub(crate) sessions: weld_session::SessionTasks,
    pub(crate) reports: crate::DiagnosticReports,
    pub(crate) diagnostic_permission: crate::pairing::DiagnosticPermission,
    collecting: Arc<AtomicBool>,
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
    operation: weld_session::Operation<IrohSourcePeer>,
    _host: Arc<HostLifetime>,
}
impl weld_hoist_encoded::PendingSource for PendingSourceAdmission {
    type Transport = IrohSourcePeer;
    fn poll(&mut self) -> Result<Option<Self::Transport>> {
        self.operation.poll()
    }
}
impl PendingSourceAdmission {
    pub fn poll(&mut self) -> Result<Option<IrohSourcePeer>> {
        self.operation.poll()
    }
    fn wait(self) -> Result<IrohSourcePeer> {
        self.operation.wait()
    }
    pub fn cancel(&mut self) {
        self.operation.cancel();
    }
}

struct AcceptGuard(Arc<AtomicBool>);
impl Drop for AcceptGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct CollectionGuard(Arc<AtomicBool>);
impl Drop for CollectionGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// One cancellable outgoing connection. Drop closes even a connected result
/// that has not yet been claimed; polling never blocks the application's thread.
pub struct PendingDestinationConnection {
    operation: weld_session::Operation<IrohDestinationPeer>,
    _host: Arc<HostLifetime>,
}
impl PendingDestinationConnection {
    pub fn poll(&mut self) -> Result<Option<IrohDestinationPeer>> {
        self.operation.poll()
    }
    fn wait(self) -> Result<IrohDestinationPeer> {
        self.operation.wait()
    }
    pub fn cancel(&mut self) {
        self.operation.cancel();
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

enum ConnectTarget {
    Address(EndpointAddr),
    Adb {
        peer: iroh::EndpointId,
        address: SocketAddr,
    },
}

enum HostCommand {
    Shutdown,
}

struct EndpointStartup {
    network: IrohNetwork,
    secret: Option<SecretKey>,
    dns: IrohDnsPolicy,
    adb_listen: Option<SocketAddr>,
    services: Option<weld_session::Registry<crate::IrohConnection>>,
}

async fn run_host(
    sessions: SessionRuntime,
    options: EndpointStartup,
    mut commands: mpsc::UnboundedReceiver<HostCommand>,
    started: std_mpsc::SyncSender<Result<BoundEndpoint, String>>,
    pairing: crate::pairing::PairingHost,
    owner: Arc<Mutex<Weak<HostLifetime>>>,
) -> Result<()> {
    let EndpointStartup {
        network,
        secret,
        dns,
        adb_listen,
        services,
    } = options;
    let secret = secret.unwrap_or_else(SecretKey::generate);
    let links = (network == IrohNetwork::Adb).then(|| adb::Manager::new(secret.public()));
    let listener = match adb_listen {
        Some(address) => Some(tokio::net::TcpListener::bind(address).await?),
        None => None,
    };
    let listening = listener
        .as_ref()
        .map(|listener| listener.local_addr())
        .transpose()?;
    let _link_tasks = links.as_ref().map(|links| links.serve(listener));
    let mut endpoint = match network {
        IrohNetwork::Direct => Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled),
        IrohNetwork::N0 => Endpoint::builder(presets::N0),
        IrohNetwork::Adb => Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .clear_relay_transports()
            .clear_address_lookup()
            .portmapper_config(PortmapperConfig::Disabled)
            .dns_resolver(DnsResolver::builder().build())
            .add_custom_transport(Arc::new(adb::Transport(
                links.as_ref().context("ADB manager missing")?.clone(),
            ))),
    };
    if network == IrohNetwork::Adb {
        tracing::info!(transport = "adb", requested_dns = ?dns, "Iroh ADB mode disables IP, relay, discovery and system DNS");
    } else if let Some(resolver) = dns.resolver() {
        endpoint = endpoint.dns_resolver(resolver);
    }
    let (services, incoming) = match services {
        Some(services) => {
            let (sender, receiver) = async_channel::bounded(1);
            drop(sender);
            (services, receiver)
        }
        None => {
            let mut services = weld_session::Registry::default();
            let incoming = weld_hoist_core::session::server::register(
                &mut services,
                crate::connection::HostServices(pairing),
            )?;
            (services, incoming)
        }
    };
    let endpoint = endpoint
        .secret_key(secret)
        .alpns(
            services
                .protocols()
                .map(|id| id.as_bytes().to_vec())
                .collect(),
        )
        .bind()
        .await
        .map_err(anyhow::Error::from)?;
    if network == IrohNetwork::N0 {
        tokio::time::timeout(N0_ONLINE_WAIT, endpoint.online())
            .await
            .context("Iroh N0 endpoint did not become online within 30 seconds")?;
    }
    anyhow::ensure!(
        network != IrohNetwork::Adb || endpoint.bound_sockets().is_empty(),
        "ADB endpoint unexpectedly bound an IP transport"
    );
    let address = endpoint.watch_addr().get();
    if network == IrohNetwork::Direct && address.ip_addrs().next().is_none() {
        bail!("Iroh endpoint published no direct listening address");
    }
    let ticket = EndpointTicket::new(address).to_string();
    sessions.listen(
        crate::incoming::IrohListener::new(endpoint.clone(), owner),
        services,
    )?;
    drop(sessions);
    started
        .send(Ok(BoundEndpoint {
            ticket,
            adb_listen: listening,
            transport: TransportResources {
                endpoint: endpoint.clone(),
                io: tokio::runtime::Handle::current(),
                incoming,
                links: links.clone(),
            },
        }))
        .map_err(|_| anyhow::anyhow!("Iroh host startup receiver disappeared"))?;

    let _ = commands.recv().await;
    endpoint.close().await;
    if let Some(links) = links {
        links.close();
    }
    Ok(())
}
