use super::*;
use crate::{
    codec::IpcCodec,
    protocol::{EventKind, Request},
};
use bytes::BytesMut;
use std::{
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream as Client,
    time::Duration,
};
use tokio_util::codec::Decoder;

// Independent wire fixtures exercise compatibility through a real client socket.
const MAGIC: &[u8; 6] = b"i3-ipc";
const MAX_PAYLOAD: usize = 1_048_576;
const MODE_EVENT: u32 = (1 << 31) | 2;

fn connect(service: &ModeService) -> Client {
    let stream = Client::connect(service.socket_path()).expect("socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    stream
}

fn packet(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = MAGIC.to_vec();
    bytes.extend((payload.len() as u32).to_ne_bytes());
    bytes.extend(kind.to_ne_bytes());
    bytes.extend(payload);
    bytes
}

fn read(client: &mut Client) -> (u32, Vec<u8>) {
    let mut header = [0; 14];
    client.read_exact(&mut header).expect("header");
    assert_eq!(&header[..6], MAGIC);
    let length = u32::from_ne_bytes(header[6..10].try_into().expect("length"));
    let kind = u32::from_ne_bytes(header[10..14].try_into().expect("kind"));
    let mut bytes = vec![0; length as usize];
    client.read_exact(&mut bytes).expect("payload");
    (kind, bytes)
}

#[test]
fn subscriptions_replay_current_mode_and_keep_partial_requests_across_events() {
    let parent = tempfile::tempdir().expect("directory");
    let service = PreparedModeService::bind_in(parent.path())
        .expect("bind")
        .start()
        .expect("service");
    let mut client = connect(&service);
    let snapshot = ModeSnapshot {
        name: "resize".into(),
        pango_markup: false,
        names: vec!["default".into(), "resize".into()],
    };
    service.publisher().publish(snapshot.clone());
    client
        .write_all(&packet(2, br#"["mode"]"#))
        .expect("subscribe");
    assert_eq!(read(&mut client), (2, br#"{"success": true}"#.to_vec()));
    let (kind, event) = read(&mut client);
    assert_eq!(kind, MODE_EVENT);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&event).expect("event")["change"],
        "resize"
    );
    let query = packet(12, b"");
    client.write_all(&query[..9]).expect("partial header");
    service.publisher().publish(ModeSnapshot {
        name: "default".into(),
        ..snapshot
    });
    assert_eq!(read(&mut client).0, MODE_EVENT);
    client.write_all(&query[9..]).expect("remaining header");
    let (kind, response) = read(&mut client);
    assert_eq!(kind, 12);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&response).expect("reply")["name"],
        "default"
    );
    let subscribe = packet(2, br#"["workspace"]"#);
    client.write_all(&subscribe[..18]).expect("partial payload");
    service.publisher().publish(ModeSnapshot {
        name: "resize".into(),
        ..ModeSnapshot::default()
    });
    assert_eq!(read(&mut client).0, MODE_EVENT);
    client
        .write_all(&subscribe[18..])
        .expect("remaining payload");
    assert_eq!(read(&mut client), (2, br#"{"success": false}"#.to_vec()));
    service.publisher().publish(ModeSnapshot::default());
    assert_eq!(read(&mut client).0, MODE_EVENT);
    client.shutdown(Shutdown::Both).expect("shutdown");
    let socket = service.socket_path();
    drop(service);
    assert!(!socket.exists());
}

#[test]
fn oversized_requests_are_isolated_and_idle_connections_stop_with_the_service() {
    let parent = tempfile::tempdir().expect("directory");
    let service = PreparedModeService::bind_in(parent.path())
        .expect("bind")
        .start()
        .expect("service");
    let mut bad = connect(&service);
    let mut bytes = MAGIC.to_vec();
    bytes.extend(((MAX_PAYLOAD + 1) as u32).to_ne_bytes());
    bytes.extend(12u32.to_ne_bytes());
    bad.write_all(&bytes).expect("invalid header");
    assert_eq!(bad.read(&mut [0; 1]).expect("closed peer"), 0);
    let mut healthy = connect(&service);
    healthy.write_all(&packet(8, b"")).expect("query");
    assert_eq!(read(&mut healthy), (8, br#"["default"]"#.to_vec()));
    drop(service);
    assert_eq!(healthy.read(&mut [0; 1]).expect("closed service"), 0);
}

#[test]
fn framing_retains_each_partial_prefix_and_consumes_one_coalesced_message() {
    let subscribe = packet(2, br#"["mode"]"#);
    for split in 0..subscribe.len() {
        let mut codec = IpcCodec;
        let mut buffer = BytesMut::from(&subscribe[..split]);
        assert_eq!(codec.decode(&mut buffer).expect("partial frame"), None);
        assert_eq!(buffer.as_ref(), &subscribe[..split]);
        buffer.extend_from_slice(&subscribe[split..]);
        let query = packet(12, b"");
        buffer.extend_from_slice(&query);
        assert_eq!(
            codec.decode(&mut buffer).expect("subscription"),
            Some(Request::Subscribe(vec![EventKind::Mode]))
        );
        assert_eq!(buffer.as_ref(), query);
        assert_eq!(
            codec.decode(&mut buffer).expect("query"),
            Some(Request::GetBindingState)
        );
        assert!(buffer.is_empty());
    }
}

#[test]
fn pipelined_queries_keep_reply_types_and_malformed_json_is_isolated() {
    let parent = tempfile::tempdir().expect("directory");
    let service = PreparedModeService::bind_in(parent.path())
        .expect("bind")
        .start()
        .expect("service");
    let mut client = connect(&service);
    let mut requests = packet(8, b"");
    requests.extend(packet(12, b""));
    requests.extend(packet(0, b"exit"));
    requests.extend(packet(99, b""));
    client.write_all(&requests).expect("pipelined requests");
    assert_eq!(read(&mut client), (8, br#"["default"]"#.to_vec()));
    assert_eq!(read(&mut client), (12, br#"{"name":"default"}"#.to_vec()));
    assert_eq!(
        read(&mut client),
        (
            0,
            br#"[{"success":false,"error":"command execution is unavailable on this endpoint"}]"#
                .to_vec()
        )
    );
    assert_eq!(
        read(&mut client),
        (
            99,
            br#"{"success":false,"error":"unsupported IPC request"}"#.to_vec()
        )
    );

    let mut bad = connect(&service);
    bad.write_all(&packet(2, br#"[{"mode":null}]"#))
        .expect("malformed subscription");
    assert_eq!(bad.read(&mut [0; 1]).expect("closed peer"), 0);
    client.write_all(&packet(12, b"")).expect("healthy query");
    assert_eq!(read(&mut client), (12, br#"{"name":"default"}"#.to_vec()));
}
