use super::*;
use tokio::time::timeout;

#[test]
fn overflow_is_counted_and_framing_is_one_bounded_record() {
    let (send, mut receive) = mpsc::channel(1);
    let link = Link {
        send,
        state: watch::channel(State::Pending).0,
        done: watch::channel(false).0,
        sent: AtomicU64::new(0),
        received: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
    };
    link.enqueue(&[1, 2, 3]).expect("enqueue");
    link.enqueue(&[4]).expect("full is counted loss");
    assert_eq!(link.dropped.load(Ordering::Relaxed), 1);
    assert_eq!(link.sent.load(Ordering::Relaxed), 1);
    assert_eq!(receive.try_recv().expect("one record"), [0, 3, 1, 2, 3]);
    assert!(link.enqueue(&vec![0; MAX_PACKET + 1]).is_err());
    drop(receive);
    assert!(link.enqueue(&[1]).is_err());
}

#[tokio::test]
async fn unadmitted_socket_expires_and_releases_its_slot() {
    let manager = Manager::new(iroh::SecretKey::generate().public());
    let (_lease, mut remote) = link(&manager).await;
    let mut byte = [0];
    assert_eq!(
        timeout(
            ADMISSION_GRACE + Duration::from_secs(2),
            remote.read(&mut byte)
        )
        .await
        .expect("expiry")
        .expect("EOF"),
        0
    );
    assert_eq!(manager.slots.available_permits(), MAX_LINKS);
}

async fn link(manager: &Arc<Manager>) -> (Lease, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let lease = manager
        .connect(listener.local_addr().expect("address"))
        .await
        .expect("link");
    let (remote, _) = listener.accept().await.expect("accept");
    (lease, remote)
}

#[tokio::test]
async fn slot_pressure_evicts_only_pending_and_waits_for_driver_cleanup() {
    let manager = Manager::new(iroh::SecretKey::generate().public());
    let mut retained = Vec::new();
    for _ in 0..MAX_LINKS {
        retained.push(link(&manager).await);
    }
    retained[0].0.link.state.send_replace(State::Admitted);
    let (_replacement, _socket) = link(&manager).await;
    assert_eq!(*retained[0].0.link.state.borrow(), State::Admitted);
    assert_eq!(*retained[1].0.link.state.borrow(), State::Closed);
    assert_eq!(manager.table.lock().expect("table").links.len(), MAX_LINKS);
    for link in manager.table.lock().expect("table").links.values() {
        link.state.send_replace(State::Admitted);
    }
    assert!(manager.reserve().await.is_err());
    manager.close();
}

#[tokio::test]
async fn eof_retires_only_one_route_and_empty_endpoint_can_receive_again() {
    let manager = Manager::new(iroh::SecretKey::generate().public());
    let mut endpoint = Transport(manager.clone()).bind().expect("bind");
    let (old, remote) = link(&manager).await;
    let old_address = old.address();
    drop(remote);
    let mut done = old.link.done.subscribe();
    timeout(Duration::from_secs(2), async {
        while !*done.borrow_and_update() {
            done.changed().await.expect("done");
        }
    })
    .await
    .expect("EOF cleanup");
    assert!(!manager.is_valid_send_addr(&old_address));
    let (new, mut remote) = link(&manager).await;
    assert_ne!(new.address(), old_address);
    remote
        .write_all(&[0, 3, 1, 2, 3])
        .await
        .expect("framed packet");
    let mut storage = [0; 16];
    let mut metas = [noq_udp::RecvMeta::default()];
    let mut infos = [RecvInfo::new(route_addr(0), None)];
    timeout(
        Duration::from_secs(2),
        std::future::poll_fn(|cx| {
            endpoint.poll_recv(
                cx,
                &mut [io::IoSliceMut::new(&mut storage)],
                &mut metas,
                &mut infos,
            )
        }),
    )
    .await
    .expect("not dead endpoint")
    .expect("receive");
    assert_eq!(&storage[..3], &[1, 2, 3]);
    assert_ne!(manager.local, new.address());
    manager.close();
}

#[tokio::test]
async fn malformed_and_truncated_framing_fail_without_unbounded_allocation() {
    for bytes in [&[0, 0][..], &[1][..], &[0, 3, 1][..]] {
        assert!(read_packet(&mut &bytes[..]).await.is_err());
    }
    assert!(
        read_packet(&mut &[][..])
            .await
            .expect("boundary EOF")
            .is_none()
    );
}
