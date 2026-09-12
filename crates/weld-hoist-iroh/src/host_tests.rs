use super::*;
use crate::rendezvous::tests::ExchangeDirectory;
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
