use super::*;
use crate::{IrohDeviceIdentity, IrohHost, rendezvous::tests::ExchangeDirectory};
use std::thread;
use weld_media::VideoCodec;

fn profile() -> IrohConnectionProfile {
    IrohConnectionProfile::new(
        iroh::SecretKey::generate()
            .public()
            .to_string()
            .parse()
            .expect("identity"),
        IrohNetwork::Direct,
        vec!["127.0.0.1:7777".parse().expect("address")],
    )
    .expect("profile")
}
fn permissions() -> DevicePermissions {
    DevicePermissions {
        browse: true,
        hoist: true,
        diagnostics: false,
    }
}
fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn invitation_requires_secret_matching_candidate_and_live_confirmation() {
    let directory = ExchangeDirectory::new();
    let authority = PairingHost::default();
    authority.enable(&directory.0).expect("storage");
    let invite = authority
        .invite(&profile(), "Laptop".into())
        .expect("invite");
    let peer = iroh::SecretKey::generate().public();
    assert!(authority.claim(peer, [0; 32], "Pixel".into()).is_err());
    let (candidate, mut approved, _) = authority
        .claim(peer, invite.token, "Pixel 8 Pro".into())
        .expect("claim");
    assert!(
        authority
            .claim(peer, invite.token, "Replay".into())
            .is_err()
    );
    assert!(
        authority
            .approve(&candidate.identity, "WRONG", permissions())
            .is_err()
    );
    assert!(authority.devices().expect("devices").is_empty());
    authority
        .approve(&candidate.identity, &candidate.verification, permissions())
        .expect("approve");
    assert!(approved.try_recv().expect("confirmed"));
    assert!(
        authority
            .claim(peer, invite.token, "Replay".into())
            .is_err()
    );
    assert_eq!(authority.devices().expect("devices")[0].name, "Pixel 8 Pro");
    drop(authority);
    let restored = PairingHost::default();
    restored.enable(&directory.0).expect("restart");
    assert_eq!(
        restored.devices().expect("persistent trust")[0].identity,
        peer.to_string()
    );
    restored.revoke(&peer.to_string()).expect("revoke");
    drop(restored);
    let restored = PairingHost::default();
    restored.enable(&directory.0).expect("restart after revoke");
    assert!(restored.devices().expect("revoked persistently").is_empty());
}

#[test]
fn expired_cancelled_and_disconnected_candidates_never_gain_trust() {
    let directory = ExchangeDirectory::new();
    let authority = PairingHost::default();
    authority.enable(&directory.0).expect("storage");
    let invite = authority
        .invite(&profile(), "Laptop".into())
        .expect("invite");
    let peer = iroh::SecretKey::generate().public();
    let (candidate, wait, _) = authority
        .claim(peer, invite.token, "Pixel".into())
        .expect("claim");
    drop(wait);
    assert!(
        authority
            .approve(&candidate.identity, &candidate.verification, permissions())
            .is_err()
    );
    authority.cancel().expect("cancel");
    assert!(authority.claim(peer, invite.token, "Pixel".into()).is_err());
    let invite = authority
        .invite(&profile(), "Laptop".into())
        .expect("new invitation");
    authority
        .0
        .lock()
        .expect("state")
        .invitation
        .as_mut()
        .expect("invite")
        .expires = Instant::now() - Duration::from_secs(1);
    assert!(authority.claim(peer, invite.token, "Pixel".into()).is_err());
    assert!(authority.devices().expect("no trust").is_empty());
}

#[test]
fn private_store_has_one_writer_and_untrusted_labels_cannot_inject_controls() {
    let directory = ExchangeDirectory::new();
    let authority = PairingHost::default();
    authority.enable(&directory.0).expect("storage");
    assert!(PairingHost::default().enable(&directory.0).is_err());
    assert!(
        authority
            .invite(&profile(), "bad\u{1b}[31mname".into())
            .is_err()
    );
    assert!("weld://pair/invalid".parse::<PairingInvitation>().is_err());
    assert!(
        format!("weld://pair/{}", "x".repeat(4096))
            .parse::<PairingInvitation>()
            .is_err()
    );
}

#[test]
fn real_endpoint_enrollment_catalogue_and_revocation_share_the_same_identity() {
    let source_dir = ExchangeDirectory::new();
    let receiver_dir = ExchangeDirectory::new();
    let source_identity = IrohDeviceIdentity::load_or_create(&source_dir.0).expect("source key");
    let receiver_identity =
        IrohDeviceIdentity::load_or_create(&receiver_dir.0).expect("receiver key");
    let source =
        IrohHost::bind_with_identity(IrohNetwork::Direct, &source_identity).expect("source");
    let receiver =
        IrohHost::bind_with_identity(IrohNetwork::Direct, &receiver_identity).expect("receiver");
    let authority = source.pairing();
    authority.enable(&source_dir.0).expect("trust store");
    let notifier = IrohNotifier::new(|| Ok(()));
    let desktop = DesktopSessions::new(notifier.clone(), notifier.clone(), VideoCodec::H264);
    authority.set_desktop(desktop.clone()).expect("desktop");
    let profile = source.connection_profile().expect("profile");
    let invitation = authority.invite(&profile, "Laptop".into()).expect("invite");
    let pairing = receiver
        .begin_pairing(invitation, "Pixel 8 Pro".into(), notifier.clone())
        .expect("connect");
    wait(|| matches!(pairing.progress(), PairingProgress::Verify { .. }));
    let candidate = authority.pending().expect("state").expect("candidate");
    let PairingProgress::Verify { code, .. } = pairing.progress() else {
        panic!("verification");
    };
    assert_eq!(candidate.verification, code);
    assert_eq!(candidate.identity, receiver_identity.public_id().as_str());
    authority
        .approve(&candidate.identity, &code, permissions())
        .expect("approval");
    wait(|| matches!(pairing.progress(), PairingProgress::Approved { .. }));
    let mut pending = receiver
        .begin_device_session(profile, vec![VideoCodec::H264], notifier)
        .expect("session");
    let mut session = None;
    wait(|| {
        session = pending.poll().expect("device poll");
        session.is_some()
    });
    let session = session.expect("session");
    let peers = desktop.take_peers();
    assert_eq!(peers.len(), 1);
    let id = peers[0].0;
    let application = ApplicationInfo {
        window: 7,
        surface: weld_client::ClientSurfaceId::new(
            weld_client::ClientId::new(weld_client::ClientSourceId::new(0), 3),
            4,
        ),
        title: "Terminal".into(),
        app_id: "foot".into(),
        available: true,
        hoisted_here: false,
    };
    desktop.publish(id, vec![application.clone()]);
    wait(|| session.applications() == vec![application.clone()]);
    let report = session
        .peer
        .diagnostics()
        .expect("recorder")
        .snapshot()
        .expect("report");
    assert_eq!(
        report.session,
        peers[0]
            .1
            .diagnostics()
            .expect("source recorder")
            .snapshot()
            .expect("report")
            .session
    );
    session
        .collect_diagnostics(report.clone())
        .expect("request without permission");
    let mut response = None;
    wait(|| {
        response = session.take_diagnostics();
        response.is_some()
    });
    assert!(
        response.take().expect("reply").is_none(),
        "pairing alone does not authorize report exchange"
    );
    authority
        .set_diagnostics(receiver_identity.public_id().as_str(), true)
        .expect("grant diagnostics");
    // The request gate deliberately limits collection to once per second.
    thread::sleep(Duration::from_millis(1050));
    session
        .collect_diagnostics(report.clone())
        .expect("authorized collection");
    wait(|| {
        response = session.take_diagnostics();
        response.is_some()
    });
    let peer_report = response.expect("reply").expect("report");
    assert_eq!(peer_report.session, report.session);
    assert_eq!(peer_report.endpoint, weld_diagnostics::Endpoint::Source);
    assert!(
        source
            .diagnostics()
            .get(report.session)
            .expect("archive")
            .peer
            .is_some()
    );
    let stranger = iroh::SecretKey::generate().public();
    assert!(
        source.diagnostics().exchange(stranger, report).is_err(),
        "unrelated peer cannot access session"
    );
    session.hoist(7).expect("request");
    wait(|| {
        for action in desktop.take_actions() {
            if let DeviceAction::Hoist {
                session,
                window,
                answer,
            } = action
            {
                assert_eq!(session, id);
                assert_eq!(window, 7);
                let _ = answer.send(true);
                return true;
            }
        }
        false
    });
    authority
        .revoke(receiver_identity.public_id().as_str())
        .expect("revoke");
    assert!(
        !peers[0].1.is_available(),
        "revocation invalidates the source handle before async readers run"
    );
    wait(|| !session.peer.is_available());
    assert!(authority.devices().expect("revoked").is_empty());
    drop(session);
    drop(peers);
    authority.shutdown();
}
