use super::*;
const WELD_ALPN: &[u8] = weld_hoist_core::session::server::DIRECT_SERVICE.as_bytes();
use crate::rendezvous::tests::ExchangeDirectory;
use std::net::Ipv4Addr;
use weld_hoist_core::session::server::SessionRuntime;
use weld_hoist_encoded::{EncodedDestinationTransport, ReceiveBudget};

#[tokio::test]
async fn application_registered_service_runs_without_hoist_or_pairing_setup() {
    use weld_session::connection::{PacketConnection, PacketReceive, PacketSend};
    let runtime = SessionRuntime::start().expect("runtime");
    let mut registry = weld_session::Registry::<crate::IrohConnection>::default();
    registry
        .register("example/echo/1", 2, |connection| async move {
            let (mut writer, mut reader) = connection.accept_control().await?;
            let request = reader.receive(64).await?;
            writer.send(&request).await?;
            writer.finish().await?;
            Ok(())
        })
        .expect("register application service");
    let host =
        IrohHost::bind_registered(&runtime, IrohNetwork::Direct, registry).expect("service host");
    let client = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .expect("client");
    let connection = client
        .connect(
            host.connection_profile()
                .expect("profile")
                .endpoint_addr()
                .expect("address"),
            b"example/echo/1",
        )
        .await
        .expect("custom service");
    let (send, receive) = connection.open_bi().await.expect("lane");
    let mut writer = crate::PacketWriter::new(send);
    let mut reader = crate::PacketReader::new(receive);
    writer
        .send(b"application-owned protocol")
        .await
        .expect("request");
    assert_eq!(
        reader.receive(64).await.expect("reply"),
        b"application-owned protocol"
    );
    drop(connection);
    client.close().await;
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for admission state"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn notifier() -> IrohNotifier {
    IrohNotifier::new(|| Ok(()))
}

#[test]
fn adb_rejects_wrong_peer_and_redials_without_replacing_another_live_peer() {
    let directory = ExchangeDirectory::new();
    let identity = IrohDeviceIdentity::load_or_create(directory.0.join("source")).expect("key");
    let source = IrohHost::bind_adb_source(
        &SessionRuntime::start().expect("session runtime"),
        &identity,
        "127.0.0.1:0".parse().expect("listen"),
    )
    .expect("source");
    let profile = source.connection_profile().expect("source profile");
    assert_ne!(profile.addresses()[0].port(), 0);
    let viewers = ["a", "b"].map(|name| {
        IrohDeviceIdentity::load_or_create(directory.0.join(name)).expect("viewer key")
    });
    let destinations = viewers
        .iter()
        .map(|identity| {
            IrohHost::bind_with_identity(
                &SessionRuntime::start().expect("session runtime"),
                IrohNetwork::Adb,
                identity,
            )
            .expect("viewer")
        })
        .collect::<Vec<_>>();
    assert!(destinations[0].connection_profile().is_err());
    let trusted =
        IrohTrustedPeers::new(viewers.iter().map(IrohDeviceIdentity::public_id).collect())
            .expect("trust");
    let mut accepting = source
        .begin_accept_trusted_source(
            trusted.clone(),
            VideoCodec::Av1,
            notifier(),
            Duration::from_secs(10),
        )
        .expect("accept");
    let rogue = IrohHost::bind(
        &SessionRuntime::start().expect("session runtime"),
        IrohNetwork::Adb,
    )
    .expect("rogue");
    assert!(
        rogue
            .begin_connect_profile(
                &profile,
                vec![VideoCodec::Av1],
                notifier(),
                Duration::from_secs(2)
            )
            .expect("rogue dial")
            .wait()
            .is_err()
    );
    let mut live = Vec::new();
    for destination in &destinations {
        let receiving = destination
            .begin_connect_profile(
                &profile,
                vec![VideoCodec::Av1],
                notifier(),
                Duration::from_secs(5),
            )
            .expect("dial")
            .wait()
            .expect("connected");
        let mut sending = None;
        wait_until(|| {
            sending = accepting.poll().expect("admitted");
            sending.is_some()
        });
        live.push((sending.expect("source"), receiving));
        accepting = source
            .begin_accept_trusted_source(
                trusted.clone(),
                VideoCodec::Av1,
                notifier(),
                Duration::from_secs(10),
            )
            .expect("rearm");
    }
    live[0].1.disconnect();
    wait_until(|| !live[0].0.is_available());
    assert!(live[1].0.is_available() && live[1].1.is_available());
    let receiving = destinations[0]
        .begin_connect_profile(
            &profile,
            vec![VideoCodec::Av1],
            notifier(),
            Duration::from_secs(5),
        )
        .expect("redial")
        .wait()
        .expect("reconnected");
    let mut sending = None;
    wait_until(|| {
        sending = accepting.poll().expect("readmitted");
        sending.is_some()
    });
    sending.expect("source").disconnect();
    receiving.disconnect();
    live[1].0.disconnect();
}

fn profile(host: &IrohHost) -> IrohConnectionProfile {
    let ticket = EndpointTicket::from_str(host.ticket()).expect("ticket");
    let address = ticket.endpoint_addr();
    let expected = IrohConnectionProfile::new(
        address.id.to_string().parse().expect("public identity"),
        IrohNetwork::Direct,
        address.ip_addrs().copied().collect(),
    )
    .expect("direct profile");
    let profile = host.connection_profile().expect("public profile API");
    assert_eq!(profile, expected);
    profile
}

#[test]
fn persisted_hosts_admit_each_trusted_device_after_restart() {
    let directory = ExchangeDirectory::new();
    let source_identity =
        IrohDeviceIdentity::load_or_create(directory.0.join("source")).expect("source key");
    let viewers = ["phone", "headset"].map(|name| {
        IrohDeviceIdentity::load_or_create(directory.0.join(name)).expect("viewer key")
    });
    let trusted = IrohTrustedPeers::new(viewers.iter().map(|key| key.public_id()).collect())
        .expect("explicit allowlist");
    for _ in 0..2 {
        let source = IrohHost::bind_with_identity(
            &SessionRuntime::start().expect("session runtime"),
            IrohNetwork::Direct,
            &source_identity,
        )
        .expect("persistent source");
        assert_eq!(profile(&source).peer(), &source_identity.public_id());
        for (viewer, dns) in viewers
            .iter()
            .zip([IrohDnsPolicy::System, IrohDnsPolicy::Public])
        {
            let destination = IrohHost::bind_with_identity_and_dns(
                &SessionRuntime::start().expect("session runtime"),
                IrohNetwork::Direct,
                viewer,
                dns,
            )
            .expect("persistent viewer with explicit DNS policy");
            assert_eq!(profile(&destination).peer(), &viewer.public_id());
            let mut accepting = source
                .begin_accept_trusted_source(
                    trusted.clone(),
                    VideoCodec::Av1,
                    notifier(),
                    Duration::from_secs(5),
                )
                .expect("re-armed admission");
            let mut connecting = destination
                .begin_connect_profile(
                    &profile(&source),
                    vec![VideoCodec::Av1],
                    notifier(),
                    Duration::from_secs(5),
                )
                .expect("profile dial");
            let mut received = None;
            wait_until(|| {
                received = connecting.poll().expect("connect result");
                received.is_some()
            });
            let received = received.expect("destination peer");
            assert_eq!(received.codec(), VideoCodec::Av1);
            assert!(connecting.poll().is_err());
            let mut sending = None;
            wait_until(|| {
                sending = accepting.poll().expect("admission result");
                sending.is_some()
            });
            sending.expect("source peer").disconnect();
            wait_until(|| received.drain(ReceiveBudget::ALL).is_err());
            wait_until(|| !source.lifetime.accepting.load(Ordering::Acquire));
        }
    }
}

#[test]
fn dropping_unclaimed_destination_result_disconnects_source() {
    let source = IrohHost::bind(
        &SessionRuntime::start().expect("session runtime"),
        IrohNetwork::Direct,
    )
    .expect("source");
    let destination = IrohHost::bind(
        &SessionRuntime::start().expect("session runtime"),
        IrohNetwork::Direct,
    )
    .expect("destination");
    let mut accepting = source
        .begin_accept_trusted_source(
            IrohTrustedPeers::new(vec![profile(&destination).peer().clone()]).expect("allowlist"),
            VideoCodec::Av1,
            notifier(),
            Duration::from_secs(5),
        )
        .expect("admission");
    let notified = Arc::new(AtomicBool::new(false));
    let wake = notified.clone();
    let connecting = destination
        .begin_connect_profile(
            &profile(&source),
            weld_hoist_protocol::presentation::ReceiverPreferences {
                codecs: vec![VideoCodec::Av1],
                stream_mode: weld_client::SurfaceStreamMode::Composited,
            },
            IrohNotifier::new(move || {
                wake.store(true, Ordering::Release);
                Ok(())
            }),
            Duration::from_secs(5),
        )
        .expect("connect");
    let mut peer = None;
    wait_until(|| {
        peer = accepting.poll().expect("admission result");
        peer.is_some()
    });
    let peer = peer.expect("source peer");
    assert_eq!(
        weld_hoist_encoded::EncodedSourceTransport::stream_mode(&peer),
        weld_client::SurfaceStreamMode::Composited
    );
    wait_until(|| notified.load(Ordering::Acquire));
    drop(connecting);
    wait_until(|| peer.drain().is_err());
}

#[test]
fn destination_connect_is_cancellable_and_does_not_enable_discovery_implicitly() {
    let source = IrohHost::bind(
        &SessionRuntime::start().expect("session runtime"),
        IrohNetwork::Direct,
    )
    .expect("source");
    let destination = IrohHost::bind(
        &SessionRuntime::start().expect("session runtime"),
        IrohNetwork::Direct,
    )
    .expect("destination");
    let mut connecting = destination
        .begin_connect_profile(
            &profile(&source),
            vec![VideoCodec::Av1],
            notifier(),
            Duration::from_secs(30),
        )
        .expect("connect without accepting");
    assert!(connecting.poll().expect("nonblocking").is_none());
    connecting.cancel();
    assert!(connecting.poll().is_err());
    let n0 =
        IrohConnectionProfile::new(profile(&source).peer().clone(), IrohNetwork::N0, Vec::new())
            .expect("N0 ID-only profile");
    assert!(
        destination
            .begin_connect_profile(
                &n0,
                vec![VideoCodec::Av1],
                notifier(),
                Duration::from_secs(5),
            )
            .is_err()
    );
    let mut timeout = destination
        .begin_connect_profile(
            &profile(&source),
            vec![VideoCodec::Av1],
            notifier(),
            Duration::from_millis(40),
        )
        .expect("bounded retry");
    wait_until(|| match timeout.poll() {
        Ok(None) => false,
        Err(_) => true,
        Ok(Some(_)) => panic!("source has no acceptor"),
    });
}

#[test]
fn dropping_destination_connect_closes_an_incomplete_handshake() {
    let destination = IrohHost::bind(
        &SessionRuntime::start().expect("session runtime"),
        IrohNetwork::Direct,
    )
    .expect("destination");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let endpoint = Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("loopback")
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![WELD_ALPN.to_vec()])
            .bind()
            .await
            .expect("raw source");
        let profile = IrohConnectionProfile::new(
            endpoint.id().to_string().parse().expect("identity"),
            IrohNetwork::Direct,
            endpoint.bound_sockets(),
        )
        .expect("source profile");
        let connecting = destination
            .begin_connect_profile(
                &profile,
                vec![VideoCodec::Av1],
                notifier(),
                Duration::from_secs(30),
            )
            .expect("connect");
        let connection = tokio::time::timeout(Duration::from_secs(5), async {
            endpoint
                .accept()
                .await
                .expect("incoming")
                .await
                .expect("TLS")
        })
        .await
        .expect("connect deadline");
        // QUIC is authenticated, but the source never sends a Weld offer.
        drop(connecting);
        tokio::time::timeout(Duration::from_secs(2), connection.closed())
            .await
            .expect("cancellation closes the remote connection promptly");
        endpoint.close().await;
    });
}

#[test]
fn pending_admission_is_pollable_exclusive_cancellable_and_timeout_bounded() {
    let directory = ExchangeDirectory::new();
    let host = IrohHost::bind(
        &SessionRuntime::start().expect("session runtime"),
        IrohNetwork::Direct,
    )
    .expect("source");
    let expected = directory.0.join("missing.identity");
    let mut pending = host
        .begin_accept_source(
            directory.0.join("first.ticket"),
            &expected,
            VideoCodec::Av1,
            notifier(),
            Duration::from_secs(30),
        )
        .expect("pending admission");
    assert!(pending.poll().expect("nonblocking poll").is_none());
    assert!(
        host.begin_accept_source(
            directory.0.join("second.ticket"),
            &expected,
            VideoCodec::Av1,
            notifier(),
            Duration::from_secs(1)
        )
        .is_err()
    );
    assert!(!directory.0.join("second.ticket").exists());
    pending.cancel();
    assert!(pending.poll().is_err());
    wait_until(|| !host.lifetime.accepting.load(Ordering::Acquire));
    let mut timeout = host
        .begin_accept_source(
            directory.0.join("timeout.ticket"),
            &expected,
            VideoCodec::Av1,
            notifier(),
            Duration::from_millis(40),
        )
        .expect("retry after cancellation");
    wait_until(|| match timeout.poll() {
        Ok(None) => false,
        Err(error) => {
            assert!(error.to_string().contains("timed out"));
            true
        }
        Ok(Some(_)) => panic!("missing identity cannot admit a peer"),
    });
}

#[test]
fn pending_admission_succeeds_and_unclaimed_ready_peers_disconnect() {
    for claim in [true, false] {
        let directory = ExchangeDirectory::new();
        let host = IrohHost::bind(
            &SessionRuntime::start().expect("session runtime"),
            IrohNetwork::Direct,
        )
        .expect("source");
        let destination = IrohHost::bind(
            &SessionRuntime::start().expect("session runtime"),
            IrohNetwork::Direct,
        )
        .expect("destination");
        let identity = directory.0.join("destination.identity");
        let ticket = directory.0.join("source.ticket");
        destination.publish_identity(&identity).expect("identity");
        let mut pending = host
            .begin_accept_source(
                &ticket,
                &identity,
                VideoCodec::Av1,
                notifier(),
                Duration::from_secs(5),
            )
            .expect("pending");
        let peer = destination
            .connect_destination(
                &ticket,
                vec![VideoCodec::Av1],
                notifier(),
                Duration::from_secs(5),
            )
            .expect("connected");
        wait_until(|| !host.lifetime.accepting.load(Ordering::Acquire));
        if claim {
            let source = pending.poll().expect("ready").expect("source peer");
            assert_eq!(source.codec(), VideoCodec::Av1);
            assert!(pending.poll().is_err());
            source.disconnect();
        } else {
            drop(pending);
        }
        wait_until(|| peer.drain(ReceiveBudget::ALL).is_err());
    }
}

#[test]
fn dropping_pending_admission_closes_an_incomplete_handshake() {
    let directory = ExchangeDirectory::new();
    let host = IrohHost::bind(
        &SessionRuntime::start().expect("session runtime"),
        IrohNetwork::Direct,
    )
    .expect("source");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .expect("raw destination");
        let identity = directory.0.join("destination.identity");
        rendezvous::publish(&identity, &endpoint.id().to_string()).expect("identity");
        let pending = host
            .begin_accept_source(
                directory.0.join("source.ticket"),
                &identity,
                VideoCodec::Av1,
                notifier(),
                Duration::from_secs(10),
            )
            .expect("pending");
        let ticket = EndpointTicket::from_str(host.ticket()).expect("ticket");
        let connection = endpoint
            .connect(ticket.endpoint_addr().clone(), WELD_ALPN)
            .await
            .expect("QUIC");
        // Source has authenticated us and sent its offer, but we never answer it.
        let (_send, _receive) =
            tokio::time::timeout(Duration::from_secs(5), connection.accept_bi())
                .await
                .expect("offer deadline")
                .expect("offer stream");
        drop(pending);
        tokio::time::timeout(Duration::from_secs(2), connection.closed())
            .await
            .expect("cancel closes handshake promptly");
        endpoint.close().await;
    });
    wait_until(|| !host.lifetime.accepting.load(Ordering::Acquire));
}
