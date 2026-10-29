use super::*;
use weld_client::{ClientId, ClientSourceId, ClientSurfaceId};
use weld_hoist_core::session::{pairing::ConnectionControl, server::SourceSessions};
use weld_hoist_iroh::{IrohDeviceIdentity, IrohSourcePeer};
use weld_hoist_protocol::device::DevicePermissions;

fn manager(directory: &std::path::Path) -> Manager {
    Manager::new(
        directory.join("client"),
        Arc::default(),
        IrohNotifier::new(|| Ok(())),
    )
    .expect("manager")
}
fn stream() -> Playback {
    Playback {
        identity: "host".into(),
        window: 7,
        instance: [1; 16],
        application: ApplicationInfo {
            window: 7,
            surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 2), 3),
            title: "Document".into(),
            app_id: "editor".into(),
            available: true,
            hoisted_here: false,
        },
    }
}
#[test]
fn recovery_expires_at_ten_seconds_and_back_cancels_it() {
    let dir = tempfile::tempdir().expect("directory");
    let mut manager = manager(dir.path());
    let now = Instant::now();
    let deadline = now + Duration::from_secs(10);
    manager.recovery = Some(Recovery {
        stream: stream(),
        until: deadline,
    });
    manager.view.selected = Some(("host".into(), 7));
    manager.view.reconnecting_stream = true;
    manager.recover_stream(deadline - Duration::from_millis(1));
    assert!(manager.view.return_to_apps.is_none());
    manager.recover_stream(deadline);
    assert_eq!(manager.view.return_to_apps, Some(("host".into(), 7)));
    assert!(manager.view.selected.is_none());
    assert!(manager.recovery.is_none());

    manager.view.return_to_apps = None;
    manager.recovery = Some(Recovery {
        stream: stream(),
        until: deadline,
    });
    manager
        .command(Command::Release {
            identity: "host".into(),
            window: 7,
        })
        .expect("back");
    manager.recover_stream(deadline);
    assert!(manager.view.return_to_apps.is_none());
    assert!(manager.recovery.is_none());
}
#[test]
fn recovery_requires_the_same_host_lifetime_and_application_incarnation() {
    let stream = stream();
    let mut application = stream.application.clone();
    application.title = "Renamed document".into();
    assert!(stream.can_resume([1; 16], &[application.clone()]));
    assert!(!stream.can_resume([2; 16], &[application.clone()]));
    application.surface = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(1), 99), 3);
    assert!(!stream.can_resume([1; 16], &[application]));
}

struct HostFixture {
    host: IrohHost,
    sessions: SourceSessions<IrohSourcePeer>,
    _directory: tempfile::TempDir,
}
impl HostFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("host directory");
        let state = directory.path().join("source");
        let identity = IrohDeviceIdentity::load_or_create(&state).expect("identity");
        let host = IrohHost::bind_with_identity(
            &SessionRuntime::start().expect("runtime"),
            IrohNetwork::Direct,
            &identity,
        )
        .expect("host");
        let sessions =
            SourceSessions::new(|| Ok(()), || Ok(()), VideoCodec::H264).expect("sessions");
        host.pairing().enable(&state).expect("trust store");
        host.pairing()
            .set_listener(sessions.clone())
            .expect("listener");
        Self {
            host,
            sessions,
            _directory: directory,
        }
    }
    fn pair(&self, manager: &mut Manager, name: &str) {
        let profile = self.host.connection_profile().expect("profile");
        let invitation = self
            .host
            .pairing()
            .invite(&profile, name.into())
            .expect("invitation");
        manager
            .command(
                ConnectionRequest::Pair {
                    invitation,
                    name: "Station test".into(),
                }
                .into(),
            )
            .expect("pair");
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut approved = false;
        loop {
            manager.poll(Instant::now());
            if !approved && let Some(candidate) = self.host.pairing().pending().expect("pending") {
                self.host
                    .pairing()
                    .approve(
                        &candidate.identity,
                        &candidate.verification,
                        DevicePermissions {
                            browse: true,
                            hoist: true,
                            ..Default::default()
                        },
                    )
                    .expect("approve");
                approved = true;
            }
            if approved && !manager.view.pairing {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "pairing stalled: {}",
                manager.view.status
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
fn until(manager: &mut Manager, mut ready: impl FnMut(&Manager) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        manager.poll(Instant::now());
        if ready(manager) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session stalled: {}",
            manager.view.status
        );
        thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn saved_hosts_autoconnect_independently_and_recover_without_repairing() {
    let first = HostFixture::new();
    let second = HostFixture::new();
    let directory = tempfile::tempdir().expect("client directory");
    let mut client = manager(directory.path());
    first.pair(&mut client, "First");
    second.pair(&mut client, "Second");
    until(&mut client, |client| {
        client.view.peers.len() == 2
            && client
                .view
                .peers
                .values()
                .all(|peer| peer.phase == Phase::Connected)
    });
    assert_eq!(
        client.endpoints.hosts.len(),
        1,
        "hosts share the discovery endpoint"
    );
    assert!(
        client.peers.values().all(|peer| peer.receiver.is_none()),
        "browsing starts no decoder"
    );
    drop(client);
    first.sessions.admissions().take();
    second.sessions.admissions().take();

    let mut client = manager(directory.path());
    until(&mut client, |client| {
        client
            .view
            .peers
            .values()
            .filter(|peer| peer.phase == Phase::Connected)
            .count()
            == 2
    });
    let mut first_admitted = first.sessions.admissions().take();
    let second_admitted = second.sessions.admissions().take();
    assert_eq!(first_admitted.len(), 1);
    assert_eq!(second_admitted.len(), 1);
    let (_, peer, _, _) = first_admitted.pop().expect("first session");
    peer.close();
    until(&mut client, |client| {
        client
            .view
            .peers
            .values()
            .any(|peer| peer.phase == Phase::Retrying)
    });
    assert!(
        second_admitted[0].1.is_available(),
        "another host's failure stays isolated"
    );
    until(&mut client, |client| {
        client
            .view
            .peers
            .values()
            .all(|peer| peer.phase == Phase::Connected)
    });
    assert_eq!(
        first.sessions.admissions().take().len(),
        1,
        "one replacement session"
    );
    assert!(
        second.sessions.admissions().take().is_empty(),
        "healthy host retained its session"
    );
    let second_id = second
        .host
        .connection_profile()
        .expect("profile")
        .peer()
        .as_str()
        .to_owned();
    client
        .command(ConnectionRequest::Connect(second_id).into())
        .expect("open host");
    client
        .command(ConnectionRequest::CancelPairing.into())
        .expect("back from pairing");
    client.poll(Instant::now());
    assert!(second_admitted[0].1.is_available());
    assert!(second.sessions.admissions().take().is_empty());
}
