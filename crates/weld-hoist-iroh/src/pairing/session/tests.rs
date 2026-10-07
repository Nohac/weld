use super::*;
use crate::{IrohConnectionProfile, IrohHost, IrohNetwork};
use iroh::endpoint::presets;
use std::{future::pending, pin::pin};
use tokio::{
    io::{AsyncWriteExt, duplex},
    time::timeout,
};
use weld_diagnostics::{Cause, Endpoint, Observation, Operation, Recorder};

const WARNING: Duration = Duration::from_millis(5);
const STALL: Duration = Duration::from_millis(25);

#[tokio::test]
async fn live_session_recovers_a_late_reply_and_owner_drop_cancels_the_next_wait() {
    let source = iroh::Endpoint::builder(presets::Minimal)
        .alpns(vec![SESSION_ALPN.to_vec()])
        .bind()
        .await
        .expect("source");
    let receiver = IrohHost::bind(IrohNetwork::Direct).expect("receiver");
    let profile = IrohConnectionProfile::new(
        source.id().to_string().parse().expect("identity"),
        IrohNetwork::Direct,
        source.bound_sockets(),
    )
    .expect("profile");
    let mut pending = receiver
        .begin_device_session(
            profile,
            vec![VideoCodec::H264],
            IrohNotifier::new(|| Ok(())),
        )
        .expect("begin session");
    let connection = source
        .accept()
        .await
        .expect("incoming")
        .await
        .expect("connection");
    let (mut send, mut recv) = connection.accept_bi().await.expect("session stream");
    let _: Hello = read_record(&mut recv).await.expect("hello");
    write_record(
        &mut send,
        &Welcome {
            codec: VideoCodec::H264,
        },
    )
    .await
    .expect("welcome");
    let (mut control, _control_input) = connection.open_bi().await.expect("control");
    control
        .write_all(b"weldctl1")
        .await
        .expect("control marker");
    let mut media = connection.open_uni().await.expect("media");
    media.write_all(b"weldmed1").await.expect("media marker");
    let session = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(session) = pending.poll().expect("poll") {
                break session;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("ready session");
    let _: Request = timeout(Duration::from_secs(2), read_record(&mut recv))
        .await
        .expect("request arrives")
        .expect("request");
    let mut reply = Vec::new();
    write_record(&mut reply, &Reply::Catalogue(Vec::new()))
        .await
        .expect("reply");
    send.write_all(&reply[..2])
        .await
        .expect("partial reply header");
    tokio::time::sleep(Duration::from_millis(5200)).await;
    assert!(
        session.peer.is_available(),
        "the old five-second deadline must preserve the peer"
    );
    send.write_all(&reply[2..]).await.expect("remaining reply");
    let _: Request = timeout(Duration::from_secs(2), read_record(&mut recv))
        .await
        .expect("polling resumes after recovery")
        .expect("next request");
    drop(session);
    timeout(Duration::from_secs(2), connection.closed())
        .await
        .expect("dropping the owner cancels the stalled session");
    source.close().await;
}

#[tokio::test]
async fn delayed_partial_records_resume_in_order_while_media_progresses() {
    let (source, receiver, connection, remote) = crate::tests::connection_pair().await;
    let recorder = Recorder::new(weld_diagnostics::SessionId([4; 16]), Endpoint::Receiver);
    let expected = vec![7_u8; 32];
    let mut bytes = Vec::new();
    write_record(&mut bytes, &expected).await.expect("frame");

    // Stall once inside the length header, then inside the record body.
    for split in [2, 8] {
        let (mut writer, mut reader) = duplex(128);
        writer.write_all(&bytes[..split]).await.expect("prefix");
        {
            let mut read = pin!(session_io(
                &connection,
                WARNING,
                Operation::SessionRead,
                Some(&recorder),
                read_record::<_, Vec<u8>>(&mut reader),
            ));
            assert!(
                timeout(STALL, &mut read).await.is_err(),
                "read must remain pending"
            );
            assert!(connection.close_reason().is_none());
            assert!(recorder.snapshot().expect("report").first_failure.is_none());

            // Independent media traffic can complete during the control stall.
            timeout(Duration::from_secs(2), async {
                let mut media = remote.open_uni().await.expect("media stream");
                media.write_all(b"live").await.expect("media write");
                media.finish().expect("media finish");
                let mut media = connection.accept_uni().await.expect("media receive");
                assert_eq!(media.read_to_end(4).await.expect("media bytes"), b"live");
            })
            .await
            .expect("media remains live");

            writer.write_all(&bytes[split..]).await.expect("suffix");
            write_record(&mut writer, &vec![9_u8])
                .await
                .expect("next record");
            assert_eq!(read.await.expect("recovered read"), expected);
        }
        assert_eq!(
            read_record::<_, Vec<u8>>(&mut reader)
                .await
                .expect("next read"),
            vec![9]
        );
    }
    assert!(recorder.snapshot().expect("report").first_failure.is_none());
    connection.close(0_u32.into(), b"test completed");
    source.close().await;
    receiver.close().await;
}

#[tokio::test]
async fn backpressured_write_resumes_without_repeating_record_prefix() {
    let (source, receiver, connection, _remote) = crate::tests::connection_pair().await;
    let recorder = Recorder::new(weld_diagnostics::SessionId([5; 16]), Endpoint::Source);
    let (mut writer, mut reader) = duplex(2);
    let expected = vec![7_u8; 32];
    {
        let mut write = pin!(session_io(
            &connection,
            WARNING,
            Operation::SessionWrite,
            Some(&recorder),
            write_record(&mut writer, &expected),
        ));
        assert!(
            timeout(STALL, &mut write).await.is_err(),
            "write must remain pending"
        );
        let (sent, received) = timeout(Duration::from_secs(2), async {
            tokio::join!(write, read_record::<_, Vec<u8>>(&mut reader))
        })
        .await
        .expect("write recovers");
        sent.expect("sent once");
        assert_eq!(received.expect("complete frame"), expected);
    }
    let (sent, received) = tokio::join!(
        write_record(&mut writer, &17_u64),
        read_record::<_, u64>(&mut reader),
    );
    sent.expect("next write");
    assert_eq!(received.expect("next read"), 17);
    assert!(recorder.snapshot().expect("report").first_failure.is_none());
    connection.close(0_u32.into(), b"test completed");
    source.close().await;
    receiver.close().await;
}

#[tokio::test]
async fn session_io_retains_real_errors_and_stops_waiting_when_connection_closes() {
    let (source, receiver, connection, remote) = crate::tests::connection_pair().await;
    let recorder = Recorder::new(weld_diagnostics::SessionId([6; 16]), Endpoint::Receiver);
    let mut malformed = &u32::MAX.to_le_bytes()[..];
    assert!(
        session_io(
            &connection,
            WARNING,
            Operation::SessionRead,
            Some(&recorder),
            read_record::<_, u64>(&mut malformed),
        )
        .await
        .is_err()
    );
    assert_eq!(
        recorder
            .snapshot()
            .expect("report")
            .first_failure
            .expect("failure")
            .observation,
        Observation::Failure {
            operation: Operation::SessionRead,
            cause: Cause::ProtocolOrIo
        }
    );

    let clean = Recorder::new(weld_diagnostics::SessionId([7; 16]), Endpoint::Receiver);
    let mut read = pin!(session_io(
        &connection,
        WARNING,
        Operation::SessionRead,
        Some(&clean),
        pending::<Result<()>>(),
    ));
    assert!(timeout(STALL, &mut read).await.is_err());
    remote.close(0_u32.into(), b"test peer disconnected");
    assert!(
        timeout(Duration::from_secs(2), read)
            .await
            .expect("close wakes control task")
            .is_err()
    );
    assert!(clean.snapshot().expect("report").first_failure.is_none());
    source.close().await;
    receiver.close().await;
}
