use super::*;
use crate::{IrohPeerIdentity, reports::Access};
use weld_diagnostics::{Endpoint, Observation};

#[tokio::test]
async fn restored_report_is_scoped_to_its_peer_and_collectable_after_reconnect() {
    let (source_endpoint, receiver_endpoint, source, receiver) =
        crate::tests::connection_pair().await;
    let source_reports = DiagnosticReports::default();
    let receiver_reports = DiagnosticReports::default();
    let source_record = source_reports
        .start(&source, Endpoint::Source, Access::Participant)
        .expect("source record");
    let receiver_record = receiver_reports
        .start(&receiver, Endpoint::Receiver, Access::Participant)
        .expect("receiver record");
    let id = source_record.session().expect("id");
    receiver_record.record(Observation::Ended);
    let restored = DiagnosticReports::default();
    let owner: IrohPeerIdentity = source_endpoint.id().to_string().parse().expect("owner");
    restored
        .restore_receiver_report(&owner, receiver_record.snapshot().expect("saved report"))
        .expect("restore");
    let stranger = iroh::SecretKey::generate().public();
    assert!(matches!(
        restored.local_for_peer(stranger, id),
        Err(CollectionStatus::Unavailable)
    ));
    assert!(matches!(
        restored.local_for_peer(source_endpoint.id(), SessionId([0; 16])),
        Err(CollectionStatus::Unavailable)
    ));
    let alpn = source.alpn().to_vec();
    source.close(0_u32.into(), b"reconnect");
    receiver.closed().await;
    assert_eq!(
        source_reports
            .collect(id)
            .await
            .expect("offline report")
            .status,
        CollectionStatus::Offline
    );

    let (source, receiver) = tokio::join!(
        async {
            source_endpoint
                .accept()
                .await
                .expect("incoming")
                .await
                .expect("accepted")
        },
        receiver_endpoint.connect(source_endpoint.addr(), &alpn),
    );
    let receiver = receiver.expect("reconnected");
    source_reports
        .start(&source, Endpoint::Source, Access::Participant)
        .expect("new source record");
    restored
        .start(&receiver, Endpoint::Receiver, Access::Participant)
        .expect("new receiver record");
    let serving = tokio::spawn(serve(restored, receiver));
    let collected = source_reports
        .collect(id)
        .await
        .expect("old session collection");
    assert_eq!(collected.status, CollectionStatus::Collected);
    let peer = collected.bundle.peer.expect("restored peer evidence");
    assert_eq!(peer.session, id);
    assert!(peer.ended);
    assert_eq!(
        source_reports.collect(id).await.expect("rate limit").status,
        CollectionStatus::Busy
    );
    source.close(0_u32.into(), b"test finished");
    serving.await.expect("service ends with connection");
    source_endpoint.close().await;
    receiver_endpoint.close().await;
}

#[tokio::test]
async fn unresponsive_diagnostic_stream_preserves_cache_and_other_streams() {
    let (source_endpoint, receiver_endpoint, source, receiver) =
        crate::tests::connection_pair().await;
    let reports = DiagnosticReports::default();
    let record = reports
        .start(&source, Endpoint::Source, Access::Participant)
        .expect("record");
    let id = record.session().expect("id");
    let mut cached = record.snapshot().expect("report");
    cached.endpoint = Endpoint::Receiver;
    reports
        .exchange(receiver_endpoint.id(), cached)
        .expect("earlier collection");
    // The peer accepts the connection but has no diagnostic-stream handler.
    let result = reports.collect(id).await.expect("bounded collection");
    assert_eq!(result.status, CollectionStatus::TimedOut);
    assert!(result.bundle.peer.is_some());
    assert!(source.close_reason().is_none());
    assert!(receiver.close_reason().is_none());
    let mut send = source.open_uni().await.expect("independent stream");
    send.write_all(b"still live").await.expect("send");
    send.finish().expect("finish");
    let mut recv = tokio::time::timeout(Duration::from_secs(1), receiver.accept_uni())
        .await
        .expect("accept deadline")
        .expect("stream");
    assert_eq!(recv.read_to_end(64).await.expect("content"), b"still live");
    source.close(0_u32.into(), b"test finished");
    source_endpoint.close().await;
    receiver_endpoint.close().await;
}
