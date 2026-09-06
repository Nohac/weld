use std::net::Ipv4Addr;

use iroh::{EndpointAddr, RelayMode, endpoint::presets};

use super::*;

async fn endpoint() -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("loopback binding")
        .relay_mode(RelayMode::Disabled)
        .alpns(vec![WELD_ALPN.to_vec()])
        .bind()
        .await
        .expect("test endpoint")
}

fn address(endpoint: &Endpoint) -> EndpointAddr {
    EndpointAddr::new(endpoint.id()).with_ip_addr(endpoint.bound_sockets()[0])
}

async fn answer(connection: &Connection) {
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .expect("authorized offer stream");
    let offer: BootstrapOffer = read_record(&mut recv).await.expect("offer");
    assert_eq!(offer.role, PeerRole::Source);
    write_record(
        &mut send,
        &BootstrapAnswer {
            revision: ProtocolRevision::CURRENT,
            role: PeerRole::Destination,
            rejection: None,
        },
    )
    .await
    .expect("answer");
}

#[tokio::test]
async fn wrong_identity_receives_no_offer_and_does_not_consume_admission() {
    let source = endpoint().await;
    let intended = endpoint().await;
    let intruder = endpoint().await;
    let listener = source.clone();
    let expected = intended.id();
    let task = tokio::spawn(async move {
        accept_source(
            &listener,
            expected,
            VideoCodec::H264,
            Instant::now() + Duration::from_secs(3),
            ATTEMPT_TIMEOUT,
        )
        .await
    });
    let wrong = intruder
        .connect(address(&source), WELD_ALPN)
        .await
        .expect("TLS peer");
    assert!(matches!(
        timeout(Duration::from_secs(1), wrong.accept_bi()).await,
        Ok(Err(_))
    ));
    let correct = intended
        .connect(address(&source), WELD_ALPN)
        .await
        .expect("intended TLS peer");
    answer(&correct).await;
    let result = task
        .await
        .expect("listener task")
        .expect("approved peer after rejection");
    assert_eq!(result.pending.connection.remote_id(), expected);
    drop(result);
    source.close().await;
    intended.close().await;
    intruder.close().await;
}

#[tokio::test]
async fn silent_bootstrap_does_not_block_concurrent_intended_attempt() {
    let source = endpoint().await;
    let intended = endpoint().await;
    let listener = source.clone();
    let expected = intended.id();
    let task = tokio::spawn(async move {
        // The silent attempt outlives the overall deadline. A sequential accept loop fails.
        accept_source(
            &listener,
            expected,
            VideoCodec::H264,
            Instant::now() + Duration::from_secs(3),
            Duration::from_secs(5),
        )
        .await
    });
    let silent = intended
        .connect(address(&source), WELD_ALPN)
        .await
        .expect("silent TLS peer");
    let (_silent_send, mut silent_recv) = silent.accept_bi().await.expect("silent offer stream");
    let _: BootstrapOffer = read_record(&mut silent_recv)
        .await
        .expect("offer before stalling");
    let correct = intended
        .connect(address(&source), WELD_ALPN)
        .await
        .expect("second TLS connection");
    answer(&correct).await;
    let result = task
        .await
        .expect("listener task")
        .expect("concurrent intended admission");
    timeout(Duration::from_secs(1), silent.closed())
        .await
        .expect("losing established connection closed");
    drop(result);
    source.close().await;
    intended.close().await;
}

#[tokio::test]
async fn stalled_candidate_times_out_then_intended_peer_can_retry() {
    let source = endpoint().await;
    let intended = endpoint().await;
    let listener = source.clone();
    let expected = intended.id();
    let task = tokio::spawn(async move {
        accept_source(
            &listener,
            expected,
            VideoCodec::H264,
            Instant::now() + Duration::from_secs(3),
            Duration::from_millis(200),
        )
        .await
    });
    let silent = intended
        .connect(address(&source), WELD_ALPN)
        .await
        .expect("silent peer");
    let (_send, mut recv) = silent.accept_bi().await.expect("offer stream");
    let _: BootstrapOffer = read_record(&mut recv).await.expect("offer");
    timeout(Duration::from_secs(1), silent.closed())
        .await
        .expect("attempt deadline closes peer");
    let correct = intended
        .connect(address(&source), WELD_ALPN)
        .await
        .expect("retry peer");
    answer(&correct).await;
    let result = task.await.expect("listener").expect("successful retry");
    drop(result);
    source.close().await;
    intended.close().await;
}

#[tokio::test]
async fn destination_deadline_closes_a_source_that_never_offers() {
    let source = endpoint().await;
    let destination = endpoint().await;
    let listener = source.clone();
    let server = tokio::spawn(async move {
        let connection = listener
            .accept()
            .await
            .expect("incoming")
            .await
            .expect("authenticated");
        timeout(Duration::from_secs(2), connection.closed())
            .await
            .expect("destination timeout closes source connection");
    });
    let result = connect_destination(
        &destination,
        EndpointTicket::new(address(&source)),
        &[VideoCodec::H264],
        Instant::now() + Duration::from_millis(200),
    )
    .await;
    assert!(result.is_err());
    server.await.expect("source task");
    source.close().await;
    destination.close().await;
}

#[tokio::test]
async fn overall_admission_deadline_closes_pending_candidates() {
    let source = endpoint().await;
    let intended = endpoint().await;
    let listener = source.clone();
    let expected = intended.id();
    let task = tokio::spawn(async move {
        accept_source(
            &listener,
            expected,
            VideoCodec::H264,
            Instant::now() + Duration::from_millis(200),
            Duration::from_secs(5),
        )
        .await
    });
    let silent = intended
        .connect(address(&source), WELD_ALPN)
        .await
        .expect("TLS peer");
    let result = task.await.expect("listener");
    assert!(result.is_err());
    timeout(Duration::from_secs(1), silent.closed())
        .await
        .expect("deadline closes pending connection");
    source.close().await;
    intended.close().await;
}
