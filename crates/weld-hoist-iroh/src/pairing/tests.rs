use super::*;
use crate::{IrohDeviceIdentity, IrohHost, rendezvous::tests::ExchangeDirectory};
use std::{thread, time::Instant};
use weld_hoist_core::session::DeviceAction;
use weld_hoist_core::session::server::SessionRuntime;
use weld_hoist_core::session::server::SourceSessions;
use weld_hoist_protocol::device::ApplicationInfo;
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
        thumbnails: true,
    }
}

#[test]
fn existing_browse_grants_require_explicit_thumbnail_consent() {
    let existing: DevicePermissions =
        serde_json::from_str(r#"{"browse":true,"hoist":true,"diagnostics":false}"#)
            .expect("existing grant");
    assert!(!existing.thumbnails);
    assert!(!DevicePermissions::default().thumbnails);
}
fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

fn service_reply<T>(mut pending: weld_hoist_core::session::Pending<T>) -> T {
    let mut result = None;
    wait(|| {
        result = pending.poll().expect("service response");
        result.is_some()
    });
    result.expect("service result")
}

#[test]
fn existing_trust_files_survive_authority_restart_and_revocation() {
    let directory = ExchangeDirectory::new();
    let authority = PairingHost::default();
    authority.enable(&directory.0).expect("storage");
    let invite = authority
        .invite(&profile(), "Laptop".into())
        .expect("invite");
    let peer = iroh::SecretKey::generate().public();
    let claim = authority
        .authority()
        .expect("authority")
        .claim(
            authenticated(peer).expect("identity"),
            invite.token,
            "Phone".into(),
        )
        .expect("claim")
        .wait_blocking()
        .expect("response")
        .expect("claimed");
    authority
        .approve(
            &claim.candidate.identity,
            &claim.candidate.verification,
            permissions(),
        )
        .expect("approve");
    assert!(claim.approval.wait_blocking().expect("confirmed"));
    drop(authority);
    let restored = PairingHost::default();
    restored.enable(&directory.0).expect("restart");
    assert_eq!(
        restored.devices().expect("devices")[0].identity,
        peer.to_string()
    );
    restored.revoke(&peer.to_string()).expect("revoke");
    drop(restored);
    let restored = PairingHost::default();
    restored.enable(&directory.0).expect("restart");
    assert!(restored.devices().expect("devices").is_empty());
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
    let source = IrohHost::bind_with_identity(
        &SessionRuntime::start().expect("session runtime"),
        IrohNetwork::Direct,
        &source_identity,
    )
    .expect("source");
    let receiver = IrohHost::bind_with_identity(
        &SessionRuntime::start().expect("session runtime"),
        IrohNetwork::Direct,
        &receiver_identity,
    )
    .expect("receiver");
    let authority = source.pairing();
    authority.enable(&source_dir.0).expect("trust store");
    let notifier = IrohNotifier::new(|| Ok(()));
    let desktop = SourceSessions::new(
        {
            let notifier = notifier.clone();
            move || notifier.notify()
        },
        {
            let notifier = notifier.clone();
            move || notifier.notify()
        },
        VideoCodec::H264,
    )
    .expect("session service");
    authority.set_listener(desktop.clone()).expect("desktop");
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
        .begin_device_session(
            profile,
            weld_hoist_protocol::presentation::ReceiverPreferences {
                codecs: vec![VideoCodec::H264],
                stream_mode: weld_client::SurfaceStreamMode::Composited,
            },
            notifier,
        )
        .expect("session");
    let mut session = None;
    wait(|| {
        session = pending.poll().expect("device poll");
        session.is_some()
    });
    let session = session.expect("session");
    let peers = desktop.admissions().take();
    assert_eq!(peers.len(), 1);
    assert_eq!(
        weld_hoist_encoded::EncodedSourceTransport::stream_mode(&peers[0].1),
        weld_client::SurfaceStreamMode::Composited
    );
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
    desktop
        .services()
        .publish(id, vec![application.clone()])
        .expect("catalogue");
    wait(|| session.control.applications() == vec![application.clone()]);
    {
        use weld_hoist_core::session::thumbnail::{
            ThumbnailImage, ThumbnailRequest, ThumbnailStatus, ThumbnailTarget,
        };
        let target = ThumbnailTarget {
            window: application.window,
            surface: application.surface,
        };
        let request = ThumbnailRequest {
            target,
            if_revision: None,
        };
        let fetch = |request| {
            let mut pending = session
                .services
                .thumbnail(request)
                .expect("thumbnail request");
            let mut response = None;
            wait(|| {
                response = pending.poll().expect("thumbnail poll");
                response.is_some()
            });
            response.expect("thumbnail reply")
        };
        assert_eq!(fetch(request).reply.status, ThumbnailStatus::Pending);
        let capture = service_reply(
            desktop
                .services()
                .take_thumbnail_capture()
                .expect("capture demand"),
        )
        .expect("capture ticket");
        let png = vec![7; 256 * 1024];
        assert!(service_reply(
            desktop
                .services()
                .complete_thumbnail_capture(
                    capture,
                    Some(ThumbnailImage::new(320, 180, png.clone()).expect("image"))
                )
                .expect("capture completion")
        ));
        let image = fetch(request);
        assert_eq!(image.image.expect("payload").png(), png);
        let ThumbnailStatus::Image { revision, .. } = image.reply.status else {
            panic!("image status");
        };
        assert!(matches!(
            fetch(ThumbnailRequest {
                if_revision: Some(revision),
                ..request
            })
            .reply
            .status,
            ThumbnailStatus::Unchanged { .. }
        ));
        session
            .control
            .release()
            .expect("control remains live after preview");
        wait(|| {
            let actions = desktop.services().take_actions();
            let ready = !actions.is_empty();
            for action in actions {
                if let DeviceAction::Release { answer, .. } = action {
                    let _ = answer.send(true);
                }
            }
            ready
        });
    }
    let report = session
        .peer
        .diagnostics()
        .expect("recorder")
        .snapshot()
        .expect("report");
    let collected = source
        .collect_diagnostics(report.session)
        .expect("desktop collection");
    assert_eq!(
        collected.status,
        weld_diagnostics::collection::CollectionStatus::Collected
    );
    assert_eq!(
        collected.bundle.peer.expect("phone report").endpoint,
        weld_diagnostics::Endpoint::Receiver
    );
    assert_eq!(
        receiver
            .collect_diagnostics(report.session)
            .expect("ungranted collection")
            .status,
        weld_diagnostics::collection::CollectionStatus::Denied
    );
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
        .control
        .collect_diagnostics(report.clone())
        .expect("request without permission");
    let mut response = None;
    wait(|| {
        response = session.control.take_diagnostics();
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
    assert_eq!(
        receiver
            .collect_diagnostics(report.session)
            .expect("granted collection")
            .status,
        weld_diagnostics::collection::CollectionStatus::Collected
    );
    session
        .control
        .collect_diagnostics(report.clone())
        .expect("authorized collection");
    wait(|| {
        response = session.control.take_diagnostics();
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
    authority
        .set_diagnostics(receiver_identity.public_id().as_str(), false)
        .expect("remove report grant");
    thread::sleep(Duration::from_millis(1050));
    let denied = receiver
        .collect_diagnostics(report.session)
        .expect("grant rechecked");
    assert_eq!(
        denied.status,
        weld_diagnostics::collection::CollectionStatus::Denied
    );
    assert!(
        denied.bundle.peer.is_some(),
        "cached evidence survives failed collection"
    );
    assert!(
        source.diagnostics().exchange(stranger, report).is_err(),
        "unrelated peer cannot access session"
    );
    session.control.hoist(7).expect("request");
    wait(|| {
        for action in desktop.services().take_actions() {
            if let DeviceAction::Hoist {
                session,
                window,
                answer,
            } = action
            {
                assert_eq!(session, id);
                assert_eq!(window, 7);
                // A busy compositor used to tear down the peer after three seconds.
                thread::sleep(Duration::from_millis(3200));
                assert!(
                    peers[0].1.is_available(),
                    "slow desktop action keeps media live"
                );
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
