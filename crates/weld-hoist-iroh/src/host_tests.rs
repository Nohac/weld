use super::*;
use crate::rendezvous::tests::ExchangeDirectory;
use std::net::Ipv4Addr;
use weld_hoist_encoded::{EncodedDestinationTransport, ReceiveBudget};

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

fn profile(host: &IrohHost) -> IrohConnectionProfile {
    let ticket = EndpointTicket::from_str(host.ticket()).expect("ticket");
    let address = ticket.endpoint_addr();
    IrohConnectionProfile::new(
        address.id.to_string().parse().expect("public identity"),
        IrohNetwork::Direct,
        address.ip_addrs().copied().collect(),
    )
    .expect("direct profile")
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
        let source = IrohHost::bind_with_identity(IrohNetwork::Direct, &source_identity)
            .expect("persistent source");
        assert_eq!(profile(&source).peer(), &source_identity.public_id());
        for viewer in &viewers {
            let destination = IrohHost::bind_with_identity(IrohNetwork::Direct, viewer)
                .expect("persistent viewer");
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
    let source = IrohHost::bind(IrohNetwork::Direct).expect("source");
    let destination = IrohHost::bind(IrohNetwork::Direct).expect("destination");
    let mut accepting = source
        .begin_accept_trusted_source(
            IrohTrustedPeers::new(vec![profile(&destination).peer().clone()]).expect("allowlist"),
            VideoCodec::Av1,
            notifier(),
            Duration::from_secs(5),
        )
        .expect("admission");
    let connecting = destination
        .begin_connect_profile(
            &profile(&source),
            vec![VideoCodec::Av1],
            notifier(),
            Duration::from_secs(5),
        )
        .expect("connect");
    let mut peer = None;
    wait_until(|| {
        peer = accepting.poll().expect("admission result");
        peer.is_some()
    });
    let peer = peer.expect("source peer");
    wait_until(|| {
        !connecting
            .result
            .as_ref()
            .expect("unclaimed result")
            .is_empty()
    });
    drop(connecting);
    wait_until(|| peer.drain().is_err());
}

#[test]
fn destination_connect_is_cancellable_and_does_not_enable_discovery_implicitly() {
    let source = IrohHost::bind(IrohNetwork::Direct).expect("source");
    let destination = IrohHost::bind(IrohNetwork::Direct).expect("destination");
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
    let destination = IrohHost::bind(IrohNetwork::Direct).expect("destination");
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
    let host = IrohHost::bind(IrohNetwork::Direct).expect("source");
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
        let host = IrohHost::bind(IrohNetwork::Direct).expect("source");
        let destination = IrohHost::bind(IrohNetwork::Direct).expect("destination");
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
    let host = IrohHost::bind(IrohNetwork::Direct).expect("source");
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
