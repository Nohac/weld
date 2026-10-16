//! Same-user control of a running Weld session and its device authority.
pub mod access;
pub mod launch;
pub mod mcp;
pub mod windows;
use anyhow::{Context, Result, ensure};
use launch::{LaunchHandler, LaunchReceipt, LaunchRequest};
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
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use weld_access::{AccessAuthority, AccessStatus};
use weld_hoist_iroh::{
    IrohDeviceIdentity, IrohHost, IrohNetwork,
    pairing::{DesktopSessions, DevicePermissions, PairedDevice, PairingCandidate},
};

#[derive(Serialize, Deserialize)]
pub enum Request {
    Window(serde_json::Value),
    Access(access::AccessRequest),
    Launch(LaunchRequest),
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
    Window(serde_json::Value),
    AccessStatus(AccessStatus),
    Launched(LaunchReceipt),
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
    let display = Path::new(session);
    if display.is_absolute() {
        let name = display
            .file_name()
            .and_then(|name| name.to_str())
            .context("invalid Wayland socket filename")?;
        validate_session(name)?;
        return Ok(display
            .parent()
            .context("Wayland socket parent absent")?
            .join("weld-control")
            .join(format!("{name}.sock")));
    }
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
    match read_bounded(&mut stream, MAX_CONTROL_RESPONSE_BYTES)? {
        Response::Error(error) => anyhow::bail!("{error}"),
        response => Ok(response),
    }
}

pub struct ControlService {
    windows: windows::WindowService,
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    _lock: fs::File,
}
impl ControlService {
    pub fn windows(&self) -> windows::WindowService {
        self.windows.clone()
    }
    pub fn start(
        session: &str,
        directory: PathBuf,
        existing: Option<IrohHost>,
        desktop: DesktopSessions,
        access: AccessAuthority,
        launch: impl FnMut(LaunchRequest) -> Result<LaunchReceipt> + Send + 'static,
    ) -> Result<Self> {
        let socket = socket_path(session)?;
        Self::start_at(
            socket,
            session,
            existing,
            Some(PairingSetup { directory, desktop }),
            access,
            Box::new(launch),
        )
    }

    /// Serves native launch requests, with reports when a transport already exists.
    pub fn start_local(
        session: &str,
        host: Option<IrohHost>,
        access: AccessAuthority,
        launch: impl FnMut(LaunchRequest) -> Result<LaunchReceipt> + Send + 'static,
    ) -> Result<Self> {
        Self::start_at(
            socket_path(session)?,
            session,
            host,
            None,
            access,
            Box::new(launch),
        )
    }

    fn start_at(
        socket: PathBuf,
        session: &str,
        existing: Option<IrohHost>,
        pairing: Option<PairingSetup>,
        access: AccessAuthority,
        launch: LaunchHandler,
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
        let windows = windows::WindowService::default();
        let window_queries = windows.clone();
        let worker = thread::Builder::new()
            .name("weld-control".into())
            .spawn(move || {
                let mut owner = Owner {
                    host: existing,
                    enabled: false,
                    pairing,
                    name: session,
                    launch,
                    access: access.clone(),
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
                let owner = Arc::new(Mutex::new(owner));
                let mut connections: Vec<JoinHandle<()>> = Vec::new();
                for stream in listener.incoming() {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(mut stream) = stream else {
                        break;
                    };
                    let mut index = 0;
                    while index < connections.len() {
                        if connections[index].is_finished() {
                            let _ = connections.swap_remove(index).join();
                        } else {
                            index += 1;
                        }
                    }
                    if connections.len() >= 16 {
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                        let _ = write(
                            &mut stream,
                            &Response::Error("control connection capacity reached".into()),
                        );
                        continue;
                    }
                    let owner = owner.clone();
                    let access = access.clone();
                    let windows = window_queries.clone();
                    match thread::Builder::new()
                        .name("weld-control-request".into())
                        .spawn(move || serve_connection(stream, owner, access, windows))
                    {
                        Ok(connection) => connections.push(connection),
                        Err(error) => eprintln!("could not start control request: {error}"),
                    }
                }
                for connection in connections {
                    let _ = connection.join();
                }
                if let Ok(owner) = owner.lock()
                    && owner.enabled
                    && let Some(host) = &owner.host
                {
                    host.pairing().shutdown();
                }
            })?;
        Ok(Self {
            windows,
            socket,
            stop,
            worker: Some(worker),
            _lock: lock,
        })
    }
}

fn serve_connection(
    mut stream: UnixStream,
    owner: Arc<Mutex<Owner>>,
    access: AccessAuthority,
    windows: windows::WindowService,
) {
    let result = (|| -> Result<Response> {
        let credentials = rustix::net::sockopt::socket_peercred(&stream)?;
        ensure!(
            credentials.uid == rustix::process::geteuid(),
            "control client belongs to another user"
        );
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let request = read(&mut stream)?;
        if let Request::Window(request) = request {
            ensure!(request.is_object(), "expected one window RPC request");
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            return Ok(Response::Window(
                runtime.block_on(windows.jsonrpc_handle_async(request)),
            ));
        }
        // Access retirement remains responsive while a launch or diagnostic
        // collection waits on the serialized service owner.
        if let Request::Access(request) = request {
            return request.execute(&access);
        }
        owner
            .lock()
            .map_err(|_| anyhow::anyhow!("control service unavailable"))?
            .request(request)
    })();
    let response = result.unwrap_or_else(|error| Response::Error(format!("{error:#}")));
    let _ = write(&mut stream, &response);
}
impl Drop for ControlService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.windows.close();
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
    access: AccessAuthority,
    launch: LaunchHandler,
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
            .context("device enrollment unavailable for this instance")?;
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
            Request::Window(_) => anyhow::bail!("window requests require the window service"),
            Request::Access(request) => request.execute(&self.access)?,
            Request::Launch(request) => Response::Launched((self.launch)(request)?),
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

const MAX_CONTROL_RESPONSE_BYTES: usize = 96 * 1024 * 1024;

fn write(stream: &mut UnixStream, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_CONTROL_RESPONSE_BYTES,
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

    fn unavailable_launch(_: LaunchRequest) -> Result<LaunchReceipt> {
        anyhow::bail!("launch unavailable in fixture")
    }

    #[test]
    fn diagnostic_queries_do_not_start_a_network_host_or_create_device_storage() {
        let directory = tempfile::tempdir().expect("directory");
        let storage = directory.path().join("devices");
        let notifier = IrohNotifier::new(|| Ok(()));
        let mut owner = Owner {
            access: AccessAuthority::new(|| {}),
            launch: Box::new(unavailable_launch),
            host: None,
            enabled: false,
            pairing: Some(PairingSetup {
                directory: storage.clone(),
                desktop: DesktopSessions::new(
                    notifier.clone(),
                    notifier,
                    weld_media::VideoCodec::H264,
                )
                .expect("session service"),
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
            DesktopSessions::new(notifier.clone(), notifier, weld_media::VideoCodec::H264)
                .expect("session service");
        let service = ControlService::start_at(
            socket.clone(),
            "test",
            Some(host.clone()),
            Some(PairingSetup {
                directory: directory.path().to_owned(),
                desktop: desktop.clone(),
            }),
            AccessAuthority::new(|| {}),
            Box::new(unavailable_launch),
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
                }),
                AccessAuthority::new(|| {}),
                Box::new(unavailable_launch),
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
        let diagnostics = ControlService::start_at(
            socket.clone(),
            "test",
            Some(host),
            None,
            AccessAuthority::new(|| {}),
            Box::new(unavailable_launch),
        )
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
                .contains("device enrollment unavailable")
        );
        drop(diagnostics);
    }

    #[test]
    fn launch_rpc_preserves_arguments_and_errors_without_starting_network_services() {
        let directory = tempfile::tempdir().expect("directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private directory");
        let socket = directory.path().join("control.sock");
        let expected_directory = directory.path().to_owned();
        let service = ControlService::start_at(
            socket.clone(),
            "test",
            None,
            None,
            AccessAuthority::new(|| {}),
            Box::new(move |request| {
                assert_eq!(request.directory, expected_directory);
                assert_eq!(request.arguments, ["space in argument", "$(literal)"]);
                ensure!(request.program == "available", "executable missing");
                Ok(LaunchReceipt { process_id: 123 })
            }),
        )
        .expect("service");
        for program in ["missing", "available"] {
            let response = call(
                &socket,
                &Request::Launch(LaunchRequest {
                    program: program.into(),
                    arguments: vec!["space in argument".into(), "$(literal)".into()],
                    directory: directory.path().into(),
                }),
            );
            if program == "missing" {
                assert!(
                    response
                        .err()
                        .expect("failure")
                        .to_string()
                        .contains("executable missing")
                );
            } else {
                assert!(matches!(
                    response.expect("launch"),
                    Response::Launched(LaunchReceipt { process_id: 123 })
                ));
            }
        }
        assert!(
            matches!(call(&socket, &Request::DiagnosticSessions).expect("diagnostics"), Response::DiagnosticSessions(sessions) if sessions.is_empty())
        );
        assert!(!directory.path().join("devices.json").exists());
        drop(service);
        assert!(!socket.exists());
    }

    #[test]
    fn access_revocation_bypasses_a_blocked_launch_request() {
        let directory = tempfile::tempdir().expect("directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private directory");
        let socket = directory.path().join("control.sock");
        let access = AccessAuthority::new(|| {});
        let permit = access.open("test-peer".into()).expect("permit");
        let (entered, started) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let service = ControlService::start_at(
            socket.clone(),
            "test",
            None,
            None,
            access,
            Box::new(move |_| {
                entered.send(()).expect("entered");
                wait.recv_timeout(Duration::from_secs(5)).expect("release");
                Ok(LaunchReceipt { process_id: 123 })
            }),
        )
        .expect("service");
        let launch_socket = socket.clone();
        let launch = thread::spawn(move || {
            call(
                &launch_socket,
                &Request::Launch(LaunchRequest {
                    program: "test".into(),
                    arguments: Vec::new(),
                    directory: PathBuf::from("/tmp"),
                }),
            )
        });
        started
            .recv_timeout(Duration::from_secs(2))
            .expect("launch started");
        let (answer, result) = std::sync::mpsc::channel();
        let revoke = thread::spawn(move || {
            let response = call(&socket, &Request::Access(access::AccessRequest::RevokeAll));
            let _ = answer.send(response);
        });
        let response = result.recv_timeout(Duration::from_secs(2));
        let revoked_before_release = !permit.is_authorized();
        release.send(()).expect("release launch");
        launch
            .join()
            .expect("launch worker")
            .expect("launch response");
        revoke.join().expect("revoke worker");
        assert!(matches!(
            response.expect("prompt revoke").expect("revoke response"),
            Response::Ok
        ));
        assert!(revoked_before_release);
        drop(service);
    }

    #[test]
    fn window_rpc_waits_for_publication_and_reports_retired_windows() {
        let directory = tempfile::tempdir().expect("directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private directory");
        let socket = directory.path().join("control.sock");
        let service = ControlService::start_at(
            socket.clone(),
            "test",
            None,
            None,
            AccessAuthority::new(|| {}),
            Box::new(unavailable_launch),
        )
        .expect("service");
        let publisher = service.windows();
        let client = windows::WindowClient::new(socket);
        assert!(client.list().is_err(), "uninstalled manager is explicit");
        publisher.publish(Vec::new()).expect("empty inventory");
        let initial = client.list().expect("initial");
        publisher.publish(Vec::new()).expect("unchanged");
        assert_eq!(client.list().expect("unchanged").revision, initial.revision);
        let waiting = client.clone();
        let wait = thread::spawn(move || waiting.wait(initial.revision, 2000));
        let window = windows::WindowInfo {
            id: "9007199254740993".into(),
            app_id: Some("test".into()),
            title: Some("Window".into()),
            workspace: Some("1".into()),
            output: Some("1".into()),
            wayland_process_id: None,
            desktop_slot: windows::Rect {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            },
            application_viewport: None,
            occupied: true,
            focused: true,
            visible: true,
            floating: false,
            control: None,
            control_key: None,
            capabilities: None,
        };
        publisher.publish(vec![window.clone()]).expect("mapped");
        assert_eq!(
            wait.join().expect("worker").expect("changed").windows,
            vec![window.clone()]
        );
        assert_eq!(client.get(window.id.clone()).expect("inspect"), window);
        let mut controlled = window.clone();
        controlled.control = Some("AbCdEf12".into());
        controlled.control_key = Some("w12".into());
        publisher
            .publish(vec![controlled.clone()])
            .expect("control");
        assert_eq!(client.resolve("w12".into()).expect("resolve"), controlled);
        controlled.occupied = false;
        controlled.control = None;
        publisher
            .publish(vec![controlled.clone()])
            .expect("vacant slot");
        assert_eq!(
            client.resolve("w12".into()).expect("vacant shorthand"),
            controlled
        );
        assert!(
            client
                .capture("w12".into())
                .err()
                .expect("vacant capture")
                .contains("waiting for application")
        );
        assert_eq!(
            client
                .find(Some("test".into()), Some(false), Some(true))
                .expect("controlled empty filter")
                .windows,
            vec![controlled.clone()]
        );
        controlled.occupied = true;
        controlled.control = Some("RsTuVw34".into());
        publisher
            .publish(vec![controlled.clone()])
            .expect("replacement");
        assert_eq!(
            client.resolve("w12".into()).expect("replacement shorthand"),
            controlled
        );
        assert!(client.resolve("AbCdEf12".into()).is_err());
        publisher.publish(Vec::new()).expect("closed");
        assert!(client.get(window.id).is_err());
        assert!(
            client
                .wait(client.list().expect("current").revision, 1)
                .is_err()
        );
        assert!(client.wait(0, 20_001).is_err());
    }

    #[test]
    fn absolute_wayland_endpoints_have_distinct_private_control_directories() {
        assert_eq!(
            socket_path("/tmp/first/wayland").expect("socket"),
            PathBuf::from("/tmp/first/weld-control/wayland.sock")
        );
        assert_eq!(
            socket_path("/tmp/second/wayland").expect("socket"),
            PathBuf::from("/tmp/second/weld-control/wayland.sock")
        );
        assert!(socket_path("/tmp/invalid\nname").is_err());
    }

    #[test]
    fn session_names_cannot_escape_the_runtime_directory() {
        for invalid in ["", "..", "../other", "/tmp/other", "a/b", "a\n"] {
            assert!(validate_session(invalid).is_err());
        }
        assert!(validate_session("weld-0").is_ok());
    }
}
