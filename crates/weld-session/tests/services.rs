use anyhow::Result;
use async_channel::{Receiver, Sender};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use weld_session::{Accepted, Listener, Registry, SessionRuntime};

struct LocalConnection(Arc<AtomicUsize>);
impl Drop for LocalConnection {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}
struct LocalListener(Receiver<Accepted<LocalConnection>>);
impl Listener for LocalListener {
    type Connection = LocalConnection;
    async fn accept(&mut self) -> Option<Result<Accepted<LocalConnection>>> {
        self.0.recv().await.ok().map(Ok)
    }
}
fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < deadline, "runtime did not progress");
        thread::sleep(Duration::from_millis(2));
    }
}
fn listener(
    runtime: &SessionRuntime,
    started: Arc<AtomicUsize>,
) -> Sender<Accepted<LocalConnection>> {
    let mut services = Registry::default();
    services
        .register("example/local-service/1", 2, move |connection| {
            let started = started.clone();
            async move {
                let _connection = connection;
                started.fetch_add(1, Ordering::AcqRel);
                std::future::pending::<Result<()>>().await
            }
        })
        .expect("register independent service");
    assert_eq!(
        services.protocols().collect::<Vec<_>>(),
        ["example/local-service/1"]
    );
    let (sender, receiver) = async_channel::bounded(8);
    runtime
        .listen(LocalListener(receiver), services)
        .expect("register local listener");
    sender
}
#[test]
fn independent_service_shares_limits_across_listeners_and_needs_no_pairing_or_codec() {
    let runtime = SessionRuntime::start().expect("runtime");
    let started = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let first = listener(&runtime, started.clone());
    let second = listener(&runtime, started.clone());
    for sender in [&first, &second] {
        assert!(
            sender
                .try_send(Accepted {
                    protocol: "example/local-service/1".into(),
                    connection: LocalConnection(dropped.clone())
                })
                .is_ok()
        );
    }
    wait(|| started.load(Ordering::Acquire) == 2);
    for protocol in ["example/local-service/1", "unknown/service"] {
        assert!(
            first
                .try_send(Accepted {
                    protocol: protocol.into(),
                    connection: LocalConnection(dropped.clone())
                })
                .is_ok()
        );
    }
    wait(|| dropped.load(Ordering::Acquire) == 2);
    assert_eq!(started.load(Ordering::Acquire), 2);
    drop(runtime);
    assert_eq!(dropped.load(Ordering::Acquire), 4);
}

#[test]
fn cancellation_unclaimed_results_and_shutdown_retire_owned_resources() {
    let runtime = SessionRuntime::start().expect("runtime");
    let dropped = Arc::new(AtomicUsize::new(0));
    let wakeups = Arc::new(AtomicUsize::new(0));
    let wake = {
        let wakes = wakeups.clone();
        Arc::new(move || {
            wakes.fetch_add(1, Ordering::AcqRel);
        })
    };
    let owned = LocalConnection(dropped.clone());
    let pending = runtime
        .start_operation::<(), _, _>(
            "wait",
            2,
            wake.clone(),
            |_| {},
            move |_| async move {
                let _owned = owned;
                std::future::pending::<Result<()>>().await
            },
        )
        .expect("pending");
    drop(pending);
    wait(|| dropped.load(Ordering::Acquire) == 1);

    let retired = Arc::new(AtomicUsize::new(0));
    let ready = retired.clone();
    let pending = runtime
        .start_operation(
            "ready",
            2,
            wake.clone(),
            |counter: &Arc<AtomicUsize>| {
                counter.fetch_add(1, Ordering::AcqRel);
            },
            move |completion| async move { completion.send(ready) },
        )
        .expect("ready operation");
    wait(|| wakeups.load(Ordering::Acquire) >= 2);
    drop(pending);
    wait(|| retired.load(Ordering::Acquire) == 1);

    let owned = LocalConnection(dropped.clone());
    let mut pending = runtime
        .start_operation::<(), _, _>(
            "wait",
            2,
            wake,
            |_| {},
            move |_| async move {
                let _owned = owned;
                std::future::pending::<Result<()>>().await
            },
        )
        .expect("shutdown operation");
    drop(runtime);
    assert_eq!(dropped.load(Ordering::Acquire), 2);
    assert!(pending.poll().is_err());
}

#[test]
fn claiming_a_result_transfers_ownership_without_cancelling_the_live_operation() {
    let runtime = SessionRuntime::start().expect("runtime");
    let retired = Arc::new(AtomicUsize::new(0));
    let owned = retired.clone();
    let (finish, finished) = async_channel::bounded(1);
    let operation = runtime
        .start_operation(
            "claim",
            1,
            Arc::new(|| {}),
            |counter: &Arc<AtomicUsize>| {
                counter.fetch_add(1, Ordering::AcqRel);
            },
            move |completion| async move {
                completion.send(owned)?;
                finished.recv().await?;
                Ok(())
            },
        )
        .expect("operation");
    let _claimed = operation.wait().expect("claim");
    assert_eq!(retired.load(Ordering::Acquire), 0);
    finish
        .send_blocking(())
        .expect("live operation survived result claim");
}

#[test]
fn overloaded_operation_never_runs_its_factory_and_notifies_its_waiter() {
    let runtime = SessionRuntime::start().expect("runtime");
    let started = Arc::new(AtomicUsize::new(0));
    let first_started = started.clone();
    let first = runtime
        .start_operation::<(), _, _>(
            "bounded-operation",
            1,
            Arc::new(|| {}),
            |_| {},
            move |_| {
                first_started.fetch_add(1, Ordering::AcqRel);
                async { std::future::pending::<Result<()>>().await }
            },
        )
        .expect("first operation");
    wait(|| started.load(Ordering::Acquire) == 1);
    let wakeups = Arc::new(AtomicUsize::new(0));
    let wake = wakeups.clone();
    let second_started = started.clone();
    let mut rejected = runtime
        .start_operation::<(), _, _>(
            "bounded-operation",
            1,
            Arc::new(move || {
                wake.fetch_add(1, Ordering::AcqRel);
            }),
            |_| {},
            move |_| {
                second_started.fetch_add(1, Ordering::AcqRel);
                async { Ok(()) }
            },
        )
        .expect("queued operation");
    wait(|| wakeups.load(Ordering::Acquire) > 0);
    assert!(rejected.poll().is_err());
    assert_eq!(started.load(Ordering::Acquire), 1);
    drop(first);
}
