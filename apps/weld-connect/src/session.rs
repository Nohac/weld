//! App-lifetime host connections, pairing, and the single active stream.
mod connection;
#[cfg(test)]
mod tests;
use crate::{
    media,
    store::{Host, Store},
};
use anyhow::{Context, Result, ensure};
use connection::Connection;
pub use connection::Phase;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use weld_client::SurfaceStreamMode;
use weld_hoist_core::session::{pairing::enrollment::PairingProgress, server::SessionRuntime};
use weld_hoist_iroh::{
    IrohConnectionProfile, IrohDnsPolicy, IrohHost, IrohNetwork, IrohNotifier,
    pairing::{DeviceSession, PairingInvitation, PendingDeviceSession, PendingPairing},
};
use weld_hoist_protocol::{device::ApplicationInfo, presentation::ReceiverPreferences};
use weld_media::VideoCodec;

#[derive(Clone, PartialEq, Eq)]
pub struct PeerSnapshot {
    pub phase: Phase,
    pub error: Option<String>,
    pub applications: Vec<ApplicationInfo>,
}
impl PeerSnapshot {
    pub fn label(&self) -> &'static str {
        match self.phase {
            Phase::Connecting => "Connecting",
            Phase::Connected => "Connected",
            Phase::Retrying => "Reconnecting",
        }
    }
}
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub hosts: Vec<Host>,
    pub peers: BTreeMap<String, PeerSnapshot>,
    pub status: String,
    pub verification: Option<(String, String)>,
    pub pairing: bool,
    pub selected: Option<(String, u64)>,
    pub stream_error: Option<String>,
    pub reconnecting_stream: bool,
    pub return_to_apps: Option<(String, u64)>,
    pub navigation_revision: u64,
}
pub enum Command {
    Connection(ConnectionRequest),
    Hoist { identity: String, window: u64 },
    Release { identity: String, window: u64 },
}
pub enum ConnectionRequest {
    Pair {
        invitation: PairingInvitation,
        name: String,
    },
    Connect(String),
    CancelPairing,
}
impl From<ConnectionRequest> for Command {
    fn from(value: ConnectionRequest) -> Self {
        Self::Connection(value)
    }
}
pub struct Session {
    pub snapshot: Arc<Mutex<Snapshot>>,
    pub media: Arc<media::Shared>,
    input: SyncSender<media::Input>,
    commands: SyncSender<Command>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Session {
    pub fn start(directory: PathBuf) -> Result<Self> {
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let (commands, receiver) = mpsc::sync_channel(16);
        let (input, events) = mpsc::sync_channel(128);
        let media = Arc::new(media::Shared::default());
        let frames = media.clone();
        let shared = snapshot.clone();
        let stop = stopped.clone();
        let worker = thread::Builder::new()
            .name("station-session".into())
            .spawn(move || {
                if let Err(error) = run(directory, &shared, &stop, receiver, frames, events)
                    && let Ok(mut view) = shared.lock()
                {
                    view.status = format!("Client stopped: {error:#}");
                    view.peers.clear();
                    view.selected = None;
                    view.stream_error = Some(view.status.clone());
                    view.pairing = false;
                }
            })?;
        Ok(Self {
            snapshot,
            media,
            input,
            commands,
            stopped,
            worker: Some(worker),
        })
    }
    pub fn send(&self, command: impl Into<Command>) -> Result<()> {
        self.commands
            .try_send(command.into())
            .map_err(|_| anyhow::anyhow!("client command queue unavailable"))?;
        self.wake();
        Ok(())
    }
    pub fn input(&self, input: media::Input) {
        if self.input.try_send(input).is_err() {
            self.media.reset.store(true, Ordering::Release);
        }
        self.wake();
    }
    pub fn wake(&self) {
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            if worker.join().is_err() {
                tracing::error!("client worker panicked");
            }
        }
    }
}
struct Peer {
    profile: IrohConnectionProfile,
    connection: Connection<PendingDeviceSession, DeviceSession>,
    receiver: Option<media::Receiver>,
}
struct Pairing {
    pending: PendingPairing,
    profile: IrohConnectionProfile,
    name: String,
}
#[derive(Clone)]
struct Playback {
    identity: String,
    window: u64,
    instance: [u8; 16],
    application: ApplicationInfo,
}
impl Playback {
    fn can_resume(&self, instance: [u8; 16], applications: &[ApplicationInfo]) -> bool {
        instance == self.instance
            && applications.iter().any(|app| {
                app.window == self.window
                    && app.surface == self.application.surface
                    && app.app_id == self.application.app_id
                    && (app.available || app.hoisted_here)
            })
    }
}
struct Recovery {
    stream: Playback,
    until: Instant,
}

/// One discovery endpoint per network profile, shared by the saved hosts.
struct Endpoints {
    hosts: Vec<(IrohNetwork, IrohHost)>,
    runtime: SessionRuntime,
}
impl Endpoints {
    fn get(&mut self, store: &Store, profile: &IrohConnectionProfile) -> Result<&IrohHost> {
        let network = profile.network();
        if !self.hosts.iter().any(|(kind, _)| *kind == network) {
            let host = IrohHost::bind_client_with_identity_and_dns(
                &self.runtime,
                network,
                &store.identity,
                dns(),
            )?;
            self.hosts.push((network, host));
        }
        self.hosts
            .iter()
            .find(|(kind, _)| *kind == network)
            .map(|(_, host)| host)
            .context("network endpoint unavailable")
    }
}
struct Manager {
    playback: Option<Playback>,
    recovery: Option<Recovery>,
    pairing: Option<Pairing>,
    peers: BTreeMap<String, Peer>,
    endpoints: Endpoints,
    store: Store,
    view: Snapshot,
    media: Arc<media::Shared>,
    notifier: IrohNotifier,
}
impl Manager {
    fn new(directory: PathBuf, media: Arc<media::Shared>, notifier: IrohNotifier) -> Result<Self> {
        let store = Store::open(&directory)?;
        let mut manager = Self {
            playback: None,
            recovery: None,
            pairing: None,
            peers: BTreeMap::new(),
            endpoints: Endpoints {
                hosts: Vec::new(),
                runtime: SessionRuntime::start()?,
            },
            view: Snapshot {
                hosts: store.hosts.clone(),
                status: "Saved hosts connect automatically".into(),
                ..Default::default()
            },
            store,
            media,
            notifier,
        };
        manager.load_hosts()?;
        Ok(manager)
    }
    fn load_hosts(&mut self) -> Result<()> {
        for host in &self.store.hosts {
            let identity = host.id()?;
            if let Some(peer) = self.peers.get_mut(&identity) {
                peer.profile = host.profile()?;
            } else {
                self.peers.insert(
                    identity,
                    Peer {
                        profile: host.profile()?,
                        connection: Connection::new(Instant::now()),
                        receiver: None,
                    },
                );
            }
        }
        self.view.hosts = self.store.hosts.clone();
        Ok(())
    }
    fn stop_stream(&mut self) {
        self.recovery = None;
        self.view.reconnecting_stream = false;
        if let Some(playback) = self.playback.take() {
            if let Some(peer) = self.peers.get_mut(&playback.identity) {
                if let Some(device) = peer.connection.connected() {
                    let _ = device.control.release();
                }
                if let Some(receiver) = &mut peer.receiver {
                    receiver.select(None);
                }
            }
            drop(playback);
        }
        self.view.selected = None;
    }
    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Connection(ConnectionRequest::Pair { invitation, name }) => {
                self.pairing = None;
                self.view.verification = None;
                self.view.pairing = false;
                let profile = invitation.profile()?;
                let host_name = invitation.host_name().to_owned();
                let host = self.endpoints.get(&self.store, &profile)?;
                let pending = host.begin_pairing(invitation, name, self.notifier.clone())?;
                self.pairing = Some(Pairing {
                    pending,
                    profile,
                    name: host_name,
                });
                self.view.pairing = true;
                self.view.status = "Waiting for pairing verification".into();
            }
            Command::Connection(ConnectionRequest::CancelPairing) => {
                self.pairing = None;
                self.view.pairing = false;
                self.view.verification = None;
                self.view.status = "Saved hosts connect automatically".into();
            }
            Command::Connection(ConnectionRequest::Connect(identity)) => {
                self.peers
                    .get_mut(&identity)
                    .context("saved host no longer exists")?
                    .connection
                    .retry(Instant::now());
            }
            Command::Hoist { identity, window } => {
                if self.view.selected.as_ref() == Some(&(identity.clone(), window)) {
                    return Ok(());
                }
                self.stop_stream();
                self.view.stream_error = None;
                self.view.return_to_apps = None;
                let result: Result<()> = (|| {
                    let peer = self
                        .peers
                        .get_mut(&identity)
                        .context("saved host no longer exists")?;
                    let device = peer.connection.connected().context(
                        "Host is reconnecting. Choose an application once it is available.",
                    )?;
                    let application = device
                        .control
                        .applications()
                        .into_iter()
                        .find(|app| app.window == window && (app.available || app.hoisted_here))
                        .context("application is no longer available")?;
                    if peer.receiver.is_none() {
                        peer.receiver = Some(media::Receiver::new(
                            device.peer.clone(),
                            self.media.clone(),
                        )?);
                    }
                    device.control.hoist(window)?;
                    if let Some(receiver) = &mut peer.receiver {
                        receiver.select(Some(application.clone()));
                    }
                    self.view.selected = Some((identity.clone(), window));
                    self.playback = Some(Playback {
                        identity,
                        window,
                        instance: device.instance,
                        application,
                    });
                    Ok(())
                })();
                if let Err(error) = &result {
                    self.view.stream_error = Some(format!("{error:#}"));
                }
                result?;
            }
            Command::Release { identity, window } => {
                if self
                    .playback
                    .as_ref()
                    .or_else(|| self.recovery.as_ref().map(|recovery| &recovery.stream))
                    .is_some_and(|stream| stream.identity == identity && stream.window == window)
                {
                    self.stop_stream();
                    self.view.stream_error = None;
                }
            }
        }
        Ok(())
    }
    fn poll(&mut self, now: Instant) {
        if let Some(pairing) = &self.pairing {
            match pairing.pending.progress() {
                PairingProgress::Connecting => {}
                PairingProgress::Verify { host, code } => {
                    self.view.verification = Some((host, code))
                }
                PairingProgress::Approved { .. } => {
                    let saved = self
                        .store
                        .remember(pairing.name.clone(), pairing.profile.clone());
                    self.pairing = None;
                    self.view.pairing = false;
                    self.view.verification = None;
                    self.view.status = match saved.and_then(|_| self.load_hosts()) {
                        Ok(()) => "Paired successfully".into(),
                        Err(error) => format!("Could not save pairing: {error:#}"),
                    };
                }
                PairingProgress::Failed(error) => {
                    self.pairing = None;
                    self.view.pairing = false;
                    self.view.verification = None;
                    self.view.status = format!("Pairing failed: {error}");
                }
            }
        }
        let mut interrupted = None;
        for (identity, peer) in &mut self.peers {
            peer.connection.poll(
                now,
                || {
                    self.endpoints
                        .get(&self.store, &peer.profile)?
                        .begin_device_session(
                            peer.profile.clone(),
                            ReceiverPreferences {
                                codecs: vec![VideoCodec::Av1, VideoCodec::H264],
                                stream_mode: SurfaceStreamMode::Composited,
                            },
                            self.notifier.clone(),
                        )
                },
                PendingDeviceSession::poll,
                |device| {
                    ensure!(
                        device.peer.is_available() && device.control.is_connected(),
                        "host disconnected"
                    );
                    Ok(())
                },
            );
            let selected = self
                .playback
                .as_ref()
                .is_some_and(|stream| stream.identity == *identity);
            let applications = if let Some(device) = peer.connection.connected() {
                if let Some(error) = device.control.take_error() {
                    if selected {
                        interrupted = Some((error, false));
                    } else {
                        self.view.status = error;
                    }
                }
                device.control.applications()
            } else {
                peer.receiver = None;
                if selected {
                    interrupted =
                        Some(("Stream interrupted. Reconnecting to the host…".into(), true));
                }
                Vec::new()
            };
            if let Some(receiver) = &mut peer.receiver {
                if let Err(error) = receiver.poll() {
                    peer.receiver = None;
                    peer.connection
                        .failed(now, format!("Playback ended: {error:#}"));
                    if selected {
                        interrupted = Some(("Playback interrupted. Reconnecting…".into(), true));
                    }
                } else if selected && receiver.requested.is_none() {
                    interrupted = Some((
                        "This application was closed or returned to the host.".into(),
                        false,
                    ));
                }
            }
            self.view.peers.insert(
                identity.clone(),
                PeerSnapshot {
                    phase: peer.connection.phase(),
                    error: peer.connection.error.clone(),
                    applications,
                },
            );
        }
        if let Some((error, retry)) = interrupted {
            let stream = self.playback.clone();
            self.stop_stream();
            self.view.stream_error = Some(error);
            if let Some(stream) = stream.filter(|_| retry) {
                self.view.selected = Some((stream.identity.clone(), stream.window));
                self.view.reconnecting_stream = true;
                self.recovery = Some(Recovery {
                    stream,
                    until: now + Duration::from_secs(10),
                });
            }
        }
        self.recover_stream(now);
    }
    fn recover_stream(&mut self, now: Instant) {
        let Some(recovery) = &self.recovery else {
            return;
        };
        if now >= recovery.until {
            let target = (recovery.stream.identity.clone(), recovery.stream.window);
            self.stop_stream();
            self.view.return_to_apps = Some(target);
            self.view.navigation_revision = self.view.navigation_revision.wrapping_add(1);
            self.view.stream_error = None;
            self.view.status =
                "Stream recovery timed out. Choose an app when the host is ready.".into();
            return;
        }
        let stream = &recovery.stream;
        let ready = self
            .peers
            .get(&stream.identity)
            .and_then(|peer| peer.connection.connected())
            .is_some_and(|device| {
                stream.can_resume(device.instance, &device.control.applications())
            });
        if ready {
            let identity = stream.identity.clone();
            let window = stream.window;
            self.view.selected = None;
            if let Err(error) = self.command(Command::Hoist {
                identity: identity.clone(),
                window,
            }) {
                self.view.stream_error = Some(format!("Could not resume: {error:#}"));
                self.view.return_to_apps = Some((identity, window));
                self.view.navigation_revision = self.view.navigation_revision.wrapping_add(1);
            }
        }
    }
}
fn run(
    directory: PathBuf,
    shared: &Mutex<Snapshot>,
    stopped: &AtomicBool,
    commands: Receiver<Command>,
    media: Arc<media::Shared>,
    input: Receiver<media::Input>,
) -> Result<()> {
    let owner = thread::current();
    let notifier = IrohNotifier::new(move || {
        owner.unpark();
        Ok(())
    });
    let mut manager = Manager::new(directory, media.clone(), notifier)?;
    *shared
        .lock()
        .map_err(|_| anyhow::anyhow!("client snapshot poisoned"))? = manager.view.clone();
    while !stopped.load(Ordering::Acquire) {
        for command in commands.try_iter() {
            if let Err(error) = manager.command(command) {
                manager.view.status = format!("{error:#}");
            }
        }
        if media.reset.load(Ordering::Acquire) {
            for _ in input.try_iter() {}
        }
        manager.poll(Instant::now());
        let mut current = shared
            .lock()
            .map_err(|_| anyhow::anyhow!("client snapshot poisoned"))?;
        if *current != manager.view {
            *current = manager.view.clone();
        }
        drop(current);
        if let Some(receiver) = manager
            .playback
            .as_ref()
            .and_then(|stream| manager.peers.get_mut(&stream.identity))
            .and_then(|peer| peer.receiver.as_mut())
        {
            if media.reset.load(Ordering::Acquire) {
                for _ in input.try_iter() {}
            }
            for event in input.try_iter() {
                receiver.input(event);
            }
        } else {
            for _ in input.try_iter() {}
        }
        let deadline = manager
            .peers
            .values()
            .filter_map(|peer| peer.connection.deadline())
            .chain(manager.recovery.as_ref().map(|recovery| recovery.until))
            .chain(
                manager
                    .peers
                    .values()
                    .filter_map(|peer| peer.receiver.as_ref().and_then(media::Receiver::deadline)),
            )
            .min();
        let idle = if manager.playback.is_some() || manager.pairing.is_some() {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        };
        thread::park_timeout(deadline.map_or(idle, |at| {
            at.saturating_duration_since(Instant::now()).min(idle)
        }));
    }
    manager.stop_stream();
    Ok(())
}
fn dns() -> IrohDnsPolicy {
    if cfg!(target_os = "android") {
        IrohDnsPolicy::Public
    } else {
        IrohDnsPolicy::System
    }
}
