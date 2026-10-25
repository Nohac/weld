use super::*;
use std::sync::atomic::AtomicUsize;

#[test]
fn handoff_wait_completes_on_notification_and_timeout_wakes_cleanup() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = wakes.clone();
    let (administrator, requests) = administration_channel(move || {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    let service = WindowService::default();
    *service.administrator.lock().expect("administrator") = Some(administrator);
    let caller = service.clone();
    let waiting =
        std::thread::spawn(move || caller.request_control("control-a".into(), Some(5000)));
    let request = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("request queued");
    assert!(
        matches!(&request.command, ControlCommand::RequestInput { control } if control == "control-a")
    );
    assert!(!waiting.is_finished(), "caller waits without polling");
    request.completion.send(Ok(())).expect("handoff");
    assert_eq!(
        waiting.join().expect("caller").expect("granted"),
        "remote input available"
    );
    assert!(!request.cancelled.load(Ordering::Acquire));

    let caller = service.clone();
    let waiting = std::thread::spawn(move || caller.request_control("control-a".into(), Some(30)));
    let request = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("second request");
    assert!(
        waiting
            .join()
            .expect("caller")
            .expect_err("timeout")
            .contains("timed out")
    );
    assert!(request.cancelled.load(Ordering::Acquire));
    assert_eq!(
        wakes.load(Ordering::Relaxed),
        3,
        "timeout wakes the idle compositor to remove its indicator"
    );
    assert!(
        service
            .request_control("control-a".into(), Some(30_001))
            .is_err()
    );
    assert!(matches!(
        requests.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
}
