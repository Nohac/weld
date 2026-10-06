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
    DiagnosticSessions,
    DiagnosticReport(weld_diagnostics::SessionId),
    CollectDiagnostics(weld_diagnostics::SessionId),
    DiagnosticsPermission {
        identity: String,
        enabled: bool,
    },
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
    DiagnosticSessions(
        Vec<(
            weld_diagnostics::SessionId,
            weld_diagnostics::Endpoint,
            bool,
            bool,
        )>,
    ),
    DiagnosticReport(Box<weld_diagnostics::ReportBundle>),
    DiagnosticCollection(Box<weld_hoist_iroh::DiagnosticCollection>),
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
    match read_bounded(&mut stream, weld_diagnostics::MAX_EXPORT_BYTES as usize)? {
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
        Self::start_at(
            socket,
            session,
            existing,
            Some(PairingSetup { directory, desktop }),
        )
    }

    /// Expose reports for an existing transport without enabling device enrollment.
    pub fn start_diagnostics(session: &str, host: IrohHost) -> Result<Self> {
        Self::start_at(socket_path(session)?, session, Some(host), None)
    }

    fn start_at(
        socket: PathBuf,
        session: &str,
        existing: Option<IrohHost>,
        pairing: Option<PairingSetup>,
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
                    host: existing,
                    enabled: false,
                    pairing,
                    name: session,
                };
                if owner
                    .pairing
                    .as_ref()
                    .is_some_and(|setup| setup.directory.join("devices.json").is_file())
                {
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
                if owner.enabled
                    && let Some(host) = owner.host
                {
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

struct PairingSetup {
    directory: PathBuf,
    desktop: DesktopSessions,
}
struct Owner {
    host: Option<IrohHost>,
    enabled: bool,
    pairing: Option<PairingSetup>,
    name: String,
}
impl Owner {
    fn host(&mut self) -> Result<&IrohHost> {
        let setup = self
            .pairing
            .as_ref()
            .context("this instance exposes diagnostics only")?;
        if !self.enabled {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(
                setup
                    .directory
                    .parent()
                    .context("device directory parent absent")?,
            )?;
        }
        if self.host.is_none() {
            let identity = IrohDeviceIdentity::load_or_create(&setup.directory)?;
            self.host = Some(IrohHost::bind_with_identity(IrohNetwork::N0, &identity)?);
        }
        let host = self.host.as_ref().context("device endpoint unavailable")?;
        if !self.enabled {
            let identity = IrohDeviceIdentity::load_or_create(&setup.directory)?;
            ensure!(
                host.connection_profile()?.peer() == &identity.public_id(),
                "existing endpoint requires its matching device directory"
            );
            host.pairing().enable(&setup.directory)?;
            host.pairing().set_desktop(setup.desktop.clone())?;
            self.enabled = true;
        }
        Ok(host)
    }
    fn request(&mut self, request: Request) -> Result<Response> {
        Ok(match request {
            Request::DiagnosticSessions => Response::DiagnosticSessions(
                self.host
                    .as_ref()
                    .map(|host| {
                        host.diagnostics()
                            .list()
                            .into_iter()
                            .map(|bundle| {
                                (
                                    bundle.local.session,
                                    bundle.local.endpoint,
                                    bundle.local.ended,
                                    bundle.peer.is_some(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            Request::DiagnosticReport(id) => Response::DiagnosticReport(Box::new(
                self.host
                    .as_ref()
                    .and_then(|host| host.diagnostics().get(id))
                    .context("diagnostic session unavailable or evicted")?,
            )),
            Request::CollectDiagnostics(id) => Response::DiagnosticCollection(Box::new(
                self.host
                    .as_ref()
                    .context("no diagnostic sessions recorded yet")?
                    .collect_diagnostics(id)?,
            )),
            Request::DiagnosticsPermission { identity, enabled } => {
                self.host()?.pairing().set_diagnostics(&identity, enabled)?;
                Response::Ok
            }
            Request::Pair => {
                let name = self.name.clone();
                let host = self.host()?;
                Response::Invitation(
                    host.pairing()
                        .invite(&host.connection_profile()?, name)?
                        .link()?,
                )
            }
            Request::Pending => Response::Pending(self.host()?.pairing().pending()?),
            Request::Approve {
                identity,
                verification,
                permissions,
            } => {
                self.host()?
                    .pairing()
                    .approve(&identity, &verification, permissions)?;
                Response::Ok
            }
            Request::Cancel => {
                self.host()?.pairing().cancel()?;
                Response::Ok
            }
            Request::Devices => Response::Devices(self.host()?.pairing().devices()?),
            Request::Revoke(identity) => {
                self.host()?.pairing().revoke(&identity)?;
                Response::Ok
            }
        })
    }
}

fn write(stream: &mut UnixStream, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() as u64 <= weld_diagnostics::MAX_EXPORT_BYTES,
        "control response too large"
    );
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}
fn read<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    read_bounded(stream, 65536)
}
fn read_bounded<T: DeserializeOwned>(stream: &mut UnixStream, limit: usize) -> Result<T> {
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    ensure!(length <= limit, "control record too large");
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
    fn diagnostic_queries_do_not_start_a_network_host_or_create_device_storage() {
        let directory = tempfile::tempdir().expect("directory");
        let storage = directory.path().join("devices");
        let notifier = IrohNotifier::new(|| Ok(()));
        let mut owner = Owner {
            host: None,
            enabled: false,
            pairing: Some(PairingSetup {
                directory: storage.clone(),
                desktop: DesktopSessions::new(
                    notifier.clone(),
                    notifier,
                    weld_media::VideoCodec::H264,
                ),
            }),
            name: "test".into(),
        };
        assert!(
            matches!(owner.request(Request::DiagnosticSessions).expect("list"), Response::DiagnosticSessions(sessions) if sessions.is_empty())
        );
        assert!(
            owner
                .request(Request::DiagnosticReport(weld_diagnostics::SessionId(
                    [0; 16]
                )))
                .is_err()
        );
        assert!(owner.host.is_none());
        assert!(!storage.exists());
    }

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
            Some(host.clone()),
            Some(PairingSetup {
                directory: directory.path().to_owned(),
                desktop: desktop.clone(),
            }),
        )
        .expect("service");
        assert!(
            ControlService::start_at(
                socket.clone(),
                "test",
                Some(host.clone()),
                Some(PairingSetup {
                    directory: directory.path().to_owned(),
                    desktop
                })
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
        let diagnostics = ControlService::start_at(socket.clone(), "test", Some(host), None)
            .expect("diagnostics-only service");
        assert!(matches!(
            call(&socket, &Request::DiagnosticSessions).expect("diagnostics"),
            Response::DiagnosticSessions(_)
        ));
        assert!(
            call(&socket, &Request::Pair)
                .err()
                .expect("no enrollment")
                .to_string()
                .contains("diagnostics only")
        );
        drop(diagnostics);
    }

    #[test]
    fn session_names_cannot_escape_the_runtime_directory() {
        for invalid in ["", "..", "../other", "/tmp/other", "a/b", "a\n"] {
            assert!(validate_session(invalid).is_err());
        }
        assert!(validate_session("weld-0").is_ok());
    }
}
