//! Same-user control of a running Weld session and its device authority.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use weld_hoist_iroh::{
    IrohDeviceIdentity, IrohHost, IrohNetwork,
    pairing::{DesktopSessions, DevicePermissions, PairedDevice, PairingCandidate},
};

#[derive(Serialize, Deserialize)]
pub enum Request {
    Pair,
    Pending,
    Approve {
        identity: String,
        verification: String,
        permissions: DevicePermissions,
    },
    Cancel,
    Devices,
    Revoke(String),
}
#[derive(Serialize, Deserialize)]
pub enum Response {
    Invitation(String),
    Pending(Option<PairingCandidate>),
    Devices(Vec<PairedDevice>),
    Ok,
    Error(String),
}

pub fn socket_path(session: &str) -> Result<PathBuf> {
    validate_session(session)?;
    let dirs = xdg::BaseDirectories::with_prefix("weld");
    Ok(dirs
        .get_runtime_directory()?
        .join("weld-control")
        .join(format!("{session}.sock")))
}
pub fn device_directory(session: &str) -> Result<PathBuf> {
    validate_session(session)?;
    let dirs = xdg::BaseDirectories::with_prefix("weld");
    Ok(dirs
        .get_data_home()
        .context("XDG data directory unavailable")?
        .join("devices")
        .join(session))
}
fn validate_session(session: &str) -> Result<()> {
    ensure!(
        !session.is_empty()
            && session.len() <= 64
            && session
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            && session != "."
            && session != "..",
        "invalid Weld session name"
    );
    Ok(())
}

pub fn call(path: &Path, request: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(path)
        .with_context(|| format!("could not reach Weld at {}", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(40)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    write(&mut stream, request)?;
    match read(&mut stream)? {
        Response::Error(error) => anyhow::bail!("{error}"),
        response => Ok(response),
    }
}

pub struct ControlService {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    _lock: fs::File,
}
impl ControlService {
    pub fn start(
        session: &str,
        directory: PathBuf,
        existing: Option<IrohHost>,
        desktop: DesktopSessions,
    ) -> Result<Self> {
        let socket = socket_path(session)?;
        Self::start_at(socket, session, directory, existing, desktop)
    }

    fn start_at(
        socket: PathBuf,
        session: &str,
        directory: PathBuf,
        existing: Option<IrohHost>,
        desktop: DesktopSessions,
    ) -> Result<Self> {
        let parent = socket.parent().context("control socket parent absent")?;
        match fs::DirBuilder::new().mode(0o700).create(parent) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let metadata = fs::symlink_metadata(parent)?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == rustix::process::geteuid().as_raw()
                && metadata.mode() & 0o777 == 0o700,
            "control directory must be private and owned by this user"
        );
        // A per-instance lock protects stale socket recovery and shutdown unlink.
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(socket.with_extension("lock"))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .context("Weld control session already running")?;
        if socket.try_exists()? {
            ensure!(
                UnixStream::connect(&socket).is_err(),
                "control socket is already active"
            );
            fs::remove_file(&socket)?;
        }
        let listener = UnixListener::bind(&socket)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let session = session.to_owned();
        let worker = thread::Builder::new()
            .name("weld-control".into())
            .spawn(move || {
                let mut owner = Owner {
                    directory,
                    host: existing,
                    enabled: false,
                    desktop,
                    name: session,
                };
                if owner.directory.join("devices.json").is_file() {
                    // Persisted approval also consents to listening on later starts.
                    // This worker never blocks the compositor's event loop.
                    if let Err(error) = owner.host() {
                        eprintln!("could not restore Weld device service: {error:#}");
                    }
                }
                for stream in listener.incoming() {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(mut stream) = stream else {
                        break;
                    };
                    let result = (|| -> Result<Response> {
                        let credentials = rustix::net::sockopt::socket_peercred(&stream)?;
                        ensure!(
                            credentials.uid == rustix::process::geteuid(),
                            "control client belongs to another user"
                        );
                        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
                        owner.request(read(&mut stream)?)
                    })();
                    let response =
                        result.unwrap_or_else(|error| Response::Error(format!("{error:#}")));
                    let _ = write(&mut stream, &response);
                }
                if let Some(host) = owner.host {
                    host.pairing().shutdown();
                }
            })?;
        Ok(Self {
            socket,
            stop,
            worker: Some(worker),
            _lock: lock,
        })
    }
}
impl Drop for ControlService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.socket);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.socket);
    }
}

struct Owner {
    directory: PathBuf,
    host: Option<IrohHost>,
    enabled: bool,
    desktop: DesktopSessions,
    name: String,
}
impl Owner {
    fn host(&mut self) -> Result<&IrohHost> {
        if !self.enabled {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(
                self.directory
                    .parent()
                    .context("device directory parent absent")?,
            )?;
        }
        if self.host.is_none() {
            let identity = IrohDeviceIdentity::load_or_create(&self.directory)?;
            self.host = Some(IrohHost::bind_with_identity(IrohNetwork::N0, &identity)?);
        }
        let host = self.host.as_ref().context("device endpoint unavailable")?;
        if !self.enabled {
            let identity = IrohDeviceIdentity::load_or_create(&self.directory)?;
            ensure!(
                host.connection_profile()?.peer() == &identity.public_id(),
                "existing endpoint requires its matching device directory"
            );
            host.pairing().enable(&self.directory)?;
            host.pairing().set_desktop(self.desktop.clone())?;
            self.enabled = true;
        }
        Ok(host)
    }
    fn request(&mut self, request: Request) -> Result<Response> {
        let name = self.name.clone();
        let host = self.host()?;
        let pairing = host.pairing();
        Ok(match request {
            Request::Pair => {
                Response::Invitation(pairing.invite(&host.connection_profile()?, name)?.link()?)
            }
            Request::Pending => Response::Pending(pairing.pending()?),
            Request::Approve {
                identity,
                verification,
                permissions,
            } => {
                pairing.approve(&identity, &verification, permissions)?;
                Response::Ok
            }
            Request::Cancel => {
                pairing.cancel()?;
                Response::Ok
            }
            Request::Devices => Response::Devices(pairing.devices()?),
            Request::Revoke(identity) => {
                pairing.revoke(&identity)?;
                Response::Ok
            }
        })
    }
}

fn write(stream: &mut UnixStream, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= 65536, "control response too large");
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}
fn read<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    ensure!(length <= 65536, "control record too large");
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use weld_hoist_iroh::IrohNotifier;

    #[test]
    fn private_control_socket_rejects_oversized_records_and_keeps_serving() {
        let directory = tempfile::tempdir().expect("directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private directory");
        let socket = directory.path().join("test.sock");
        let identity = IrohDeviceIdentity::load_or_create(directory.path()).expect("identity");
        let host = IrohHost::bind_with_identity(IrohNetwork::Direct, &identity).expect("endpoint");
        let notifier = IrohNotifier::new(|| Ok(()));
        let desktop =
            DesktopSessions::new(notifier.clone(), notifier, weld_media::VideoCodec::H264);
        let service = ControlService::start_at(
            socket.clone(),
            "test",
            directory.path().to_owned(),
            Some(host.clone()),
            desktop.clone(),
        )
        .expect("service");
        assert!(
            ControlService::start_at(
                socket.clone(),
                "test",
                directory.path().to_owned(),
                Some(host),
                desktop
            )
            .is_err()
        );
        let mut invalid = UnixStream::connect(&socket).expect("socket");
        invalid
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        invalid
            .write_all(&65537u32.to_le_bytes())
            .expect("oversized length");
        assert!(matches!(
            read::<Response>(&mut invalid).expect("rejection"),
            Response::Error(_)
        ));
        assert!(
            matches!(call(&socket, &Request::Devices).expect("still serves"), Response::Devices(devices) if devices.is_empty())
        );
        let Response::Invitation(link) = call(&socket, &Request::Pair).expect("pair") else {
            panic!("invitation");
        };
        let invitation: weld_hoist_iroh::pairing::PairingInvitation =
            link.parse().expect("invitation");
        assert_eq!(
            invitation.profile().expect("profile").peer(),
            &identity.public_id()
        );
        call(&socket, &Request::Cancel).expect("cancel");
        drop(service);
        assert!(!socket.exists());
    }

    #[test]
    fn session_names_cannot_escape_the_runtime_directory() {
        for invalid in ["", "..", "../other", "/tmp/other", "a/b", "a\n"] {
            assert!(validate_session(invalid).is_err());
        }
        assert!(validate_session("weld-0").is_ok());
    }
}
