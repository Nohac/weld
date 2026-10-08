//! UI commands and bounded snapshots around the shared Iroh pairing/catalogue APIs.
use crate::media;
use crate::store::{Host, Store};
use anyhow::{Context, Result};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use weld_client::SurfaceStreamMode;
use weld_hoist_iroh::{
    IrohConnectionProfile, IrohDnsPolicy, IrohHost, IrohNotifier, IrohReceiverPreferences,
    pairing::{
        ApplicationInfo, DeviceSession, PairingInvitation, PairingProgress, PendingDeviceSession,
        PendingPairing,
    },
};
use weld_media::VideoCodec;

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub hosts: Vec<Host>,
    pub connected: Option<String>,
    pub applications: Vec<ApplicationInfo>,
    pub status: String,
    pub verification: Option<(String, String)>,
    pub pairing: bool,
    pub selected: Option<u64>,
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
    Disconnect,
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
            .name("connect-session".into())
            .spawn(move || {
                if let Err(error) = run(directory, &shared, &stop, receiver, frames, events)
                    && let Ok(mut view) = shared.lock()
                {
                    view.status = format!("Client stopped: {error:#}");
                    view.connected = None;
                    view.applications.clear();
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
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
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
enum Connection {
    Idle,
    Pairing {
        pending: PendingPairing,
        profile: IrohConnectionProfile,
        name: String,
        _host: IrohHost,
    },
    Connecting {
        pending: PendingDeviceSession,
        identity: String,
        _host: IrohHost,
    },
    Connected {
        receiver: Box<media::Receiver>,
        device: DeviceSession,
        identity: String,
        _host: IrohHost,
    },
}
fn run(
    directory: PathBuf,
    shared: &Mutex<Snapshot>,
    stopped: &AtomicBool,
    commands: Receiver<Command>,
    media: Arc<media::Shared>,
    input: Receiver<media::Input>,
) -> Result<()> {
    let mut store = Store::open(&directory)?;
    let mut view = Snapshot {
        hosts: store.hosts.clone(),
        status: "Choose a host or pair a device".into(),
        ..Default::default()
    };
    let mut connection = Connection::Idle;
    let owner = thread::current();
    let notifier = IrohNotifier::new(move || {
        owner.unpark();
        Ok(())
    });
    while !stopped.load(Ordering::Acquire) {
        for command in commands.try_iter() {
            let command = match command {
                Command::Connection(command) => command,
                command => {
                    if let Err(error) = stream_command(command, &mut connection, &mut view) {
                        view.status = error.to_string();
                    }
                    continue;
                }
            };
            connection = Connection::Idle;
            media.clear();
            view.selected = None;
            view.connected = None;
            view.applications.clear();
            view.verification = None;
            view.pairing = false;
            match start(command, &store, &notifier, &mut view) {
                Ok(next) => connection = next,
                Err(error) => view.status = format!("Could not connect: {error:#}"),
            }
        }
        if media.reset.load(Ordering::Acquire) {
            for _ in input.try_iter() {}
        }
        if let Err(error) = poll(&mut connection, &mut store, &mut view, &media) {
            connection = Connection::Idle;
            view.connected = None;
            view.applications.clear();
            view.verification = None;
            view.pairing = false;
            view.selected = None;
            view.status = format!("Connection ended: {error:#}");
        }
        let mut current = shared
            .lock()
            .map_err(|_| anyhow::anyhow!("client snapshot poisoned"))?;
        if *current != view {
            *current = view.clone();
        }
        drop(current);
        let mut deadline = None;
        if let Connection::Connected { receiver, .. } = &mut connection {
            if media.reset.load(Ordering::Acquire) {
                for _ in input.try_iter() {}
            }
            for event in input.try_iter() {
                receiver.input(event);
            }
            deadline = receiver.deadline();
        } else {
            for _ in input.try_iter() {}
        }
        thread::park_timeout(deadline.map_or(Duration::from_millis(100), |at| {
            at.saturating_duration_since(std::time::Instant::now())
                .min(Duration::from_millis(100))
        }));
    }
    Ok(())
}
fn start(
    command: ConnectionRequest,
    store: &Store,
    notifier: &IrohNotifier,
    view: &mut Snapshot,
) -> Result<Connection> {
    match command {
        ConnectionRequest::Pair { invitation, name } => {
            let profile = invitation.profile()?;
            let host =
                IrohHost::bind_with_identity_and_dns(profile.network(), &store.identity, dns())?;
            let host_name = invitation.host_name().to_owned();
            let pending = host.begin_pairing(invitation, name, notifier.clone())?;
            view.status = "Waiting for pairing verification".into();
            view.pairing = true;
            Ok(Connection::Pairing {
                pending,
                profile,
                name: host_name,
                _host: host,
            })
        }
        ConnectionRequest::Connect(identity) => {
            let profile = store
                .hosts
                .iter()
                .find(|host| host.id().is_ok_and(|id| id == identity))
                .context("saved host no longer exists")?
                .profile()?;
            let host =
                IrohHost::bind_with_identity_and_dns(profile.network(), &store.identity, dns())?;
            let pending = host.begin_device_session(
                profile,
                IrohReceiverPreferences {
                    codecs: vec![VideoCodec::Av1, VideoCodec::H264],
                    stream_mode: SurfaceStreamMode::Composited,
                },
                notifier.clone(),
            )?;
            view.status = "Connecting to host".into();
            Ok(Connection::Connecting {
                pending,
                identity,
                _host: host,
            })
        }
        ConnectionRequest::Disconnect => {
            view.status = "Choose a host".into();
            Ok(Connection::Idle)
        }
    }
}
fn poll(
    connection: &mut Connection,
    store: &mut Store,
    view: &mut Snapshot,
    media: &Arc<media::Shared>,
) -> Result<()> {
    match connection {
        Connection::Idle => {}
        Connection::Pairing {
            pending,
            profile,
            name,
            ..
        } => match pending.progress() {
            PairingProgress::Connecting => {}
            PairingProgress::Verify { host, code } => {
                view.verification = Some((host, code));
            }
            PairingProgress::Approved { .. } => {
                store.remember(name.clone(), profile.clone())?;
                view.hosts = store.hosts.clone();
                view.pairing = false;
                view.verification = None;
                view.status = "Paired. Open the host to browse applications.".into();
                *connection = Connection::Idle;
            }
            PairingProgress::Failed(error) => anyhow::bail!("{error}"),
        },
        Connection::Connecting { pending, .. } => {
            if let Some(device) = pending.poll()? {
                let previous = std::mem::replace(connection, Connection::Idle);
                if let Connection::Connecting {
                    identity, _host, ..
                } = previous
                {
                    view.connected = Some(identity.clone());
                    view.status = "Connected".into();
                    *connection = Connection::Connected {
                        receiver: Box::new(media::Receiver::new(
                            device.peer.clone(),
                            media.clone(),
                        )?),
                        device,
                        identity,
                        _host,
                    };
                }
            }
        }
        Connection::Connected {
            device,
            identity,
            receiver,
            ..
        } => {
            anyhow::ensure!(device.peer.is_available(), "host disconnected");
            if let Some(error) = device.take_error() {
                anyhow::bail!("{error}");
            }
            view.connected = Some(identity.clone());
            view.applications = device.applications();
            receiver.poll()?;
            view.selected = receiver
                .requested
                .as_ref()
                .map(|application| application.window);
        }
    }
    Ok(())
}
fn stream_command(
    command: Command,
    connection: &mut Connection,
    view: &mut Snapshot,
) -> Result<()> {
    let Connection::Connected {
        device,
        identity,
        receiver,
        ..
    } = connection
    else {
        anyhow::bail!("host is disconnected");
    };
    match command {
        Command::Hoist {
            identity: target,
            window,
        } if *identity == target => {
            let application = device
                .applications()
                .into_iter()
                .find(|app| app.window == window && (app.available || app.hoisted_here))
                .context("application is no longer available")?;
            device.hoist(window)?;
            receiver.select(Some(application));
            view.selected = Some(window);
        }
        Command::Release {
            identity: target,
            window,
        } if *identity == target
            && receiver
                .requested
                .as_ref()
                .is_some_and(|app| app.window == window) =>
        {
            device.release()?;
            receiver.select(None);
            view.selected = None;
        }
        _ => {}
    }
    Ok(())
}
fn dns() -> IrohDnsPolicy {
    if cfg!(target_os = "android") {
        IrohDnsPolicy::Public
    } else {
        IrohDnsPolicy::System
    }
}
