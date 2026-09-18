//! The public pool API accepts worker-local native owners and portable outputs.

#![cfg(feature = "decode")]

use std::{
    cell::Cell,
    collections::VecDeque,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Sender},
    },
    task::Poll,
    thread::{self, ThreadId},
    time::Duration,
};

use anyhow::{Context, Result};
use weld_media::{
    MediaFrameId, MediaStreamId, StreamGeneration,
    decode::{DecodeJob, DecodePool, DecodePoolLimits, DecodeProcessor},
};

struct Request {
    token: u64,
    frame: MediaFrameId,
}

impl DecodeJob for Request {
    fn token(&self) -> u64 {
        self.token
    }

    fn frame(&self) -> MediaFrameId {
        self.frame
    }
}

// Deliberately Send but not Sync: output ownership moves to the consumer.
struct OutputLease {
    value: Cell<u64>,
    releases: Arc<AtomicUsize>,
}

impl Drop for OutputLease {
    fn drop(&mut self) {
        self.releases.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug, PartialEq)]
enum Lifecycle {
    Created(ThreadId),
    Retired(MediaStreamId, StreamGeneration),
    Dropped(ThreadId),
}

struct LocalProcessor {
    // Rc makes the processor neither Send nor Sync, as native codec owners may be.
    owner: Rc<ThreadId>,
    pending: VecDeque<Request>,
    releases: Arc<AtomicUsize>,
    lifecycle: Sender<Lifecycle>,
}

impl DecodeProcessor for LocalProcessor {
    type Request = Request;
    type Output = OutputLease;

    fn submit(&mut self, request: Request) {
        assert_eq!(*self.owner, thread::current().id());
        self.pending.push_back(request);
    }

    fn poll(&mut self) -> Result<Poll<Vec<OutputLease>>> {
        assert_eq!(*self.owner, thread::current().id());
        let request = self.pending.pop_front().context("missing test request")?;
        // One completion may contain multiple independently owned outputs.
        Ok(Poll::Ready(
            (0..2)
                .map(|index| OutputLease {
                    value: Cell::new(request.token + index),
                    releases: self.releases.clone(),
                })
                .collect(),
        ))
    }

    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) {
        assert_eq!(*self.owner, thread::current().id());
        assert!(self.pending.is_empty());
        self.lifecycle
            .send(Lifecycle::Retired(stream, generation))
            .expect("lifecycle receiver");
    }
}

impl Drop for LocalProcessor {
    fn drop(&mut self) {
        assert_eq!(*self.owner, thread::current().id());
        self.lifecycle
            .send(Lifecycle::Dropped(thread::current().id()))
            .expect("lifecycle receiver");
    }
}

#[test]
fn native_owner_stays_on_worker_and_outputs_survive_retirement_and_pool_drop() {
    fn assert_send<T: Send>() {}
    assert_send::<DecodePool<LocalProcessor>>();

    let releases = Arc::new(AtomicUsize::new(0));
    let worker_releases = releases.clone();
    let (lifecycle_sender, lifecycle) = mpsc::channel();
    let (wake_sender, wakes) = mpsc::channel();
    let mut pool = DecodePool::new(
        DecodePoolLimits::try_new(1, 1, 1, 1).expect("limits"),
        move || {
            let owner = thread::current().id();
            lifecycle_sender.send(Lifecycle::Created(owner))?;
            Ok(LocalProcessor {
                owner: Rc::new(owner),
                pending: VecDeque::new(),
                releases: worker_releases.clone(),
                lifecycle: lifecycle_sender.clone(),
            })
        },
        move || {
            let _ = wake_sender.send(());
        },
    );
    let stream = MediaStreamId::new(1);
    let generation = StreamGeneration::new(2);
    pool.try_decode(Request {
        token: 42,
        frame: MediaFrameId::new(stream, generation, 0),
    })
    .expect("admit");

    let outputs = loop {
        let (mut completed, failure) = pool.drain();
        assert!(failure.is_none(), "{failure:?}");
        if let Some(completion) = completed.pop() {
            assert_eq!(completion.token, 42);
            break completion.result.expect("outputs");
        }
        wakes.recv_timeout(Duration::from_secs(2)).expect("wake");
    };
    let Lifecycle::Created(owner) = lifecycle
        .recv_timeout(Duration::from_secs(2))
        .expect("creation")
    else {
        panic!("unexpected lifecycle event");
    };
    assert_ne!(owner, thread::current().id());
    pool.retire(stream, generation).expect("retire");
    assert_eq!(
        lifecycle
            .recv_timeout(Duration::from_secs(2))
            .expect("retirement"),
        Lifecycle::Retired(stream, generation)
    );
    drop(pool);
    assert_eq!(
        lifecycle
            .recv_timeout(Duration::from_secs(2))
            .expect("destruction"),
        Lifecycle::Dropped(owner)
    );
    assert_eq!(releases.load(Ordering::SeqCst), 0);
    assert_eq!(
        outputs
            .iter()
            .map(|output| output.value.get())
            .collect::<Vec<_>>(),
        vec![42, 43]
    );
    drop(outputs);
    assert_eq!(releases.load(Ordering::SeqCst), 2);
}
