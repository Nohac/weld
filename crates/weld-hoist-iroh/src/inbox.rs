//! Bounded async admission into the compositor's synchronously drained inbox.
//! Each channel owns its capacity; draining never waits on a codec.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use crate::{IrohNotifier, peer::QUEUE_CAPACITY};

pub(super) struct IncomingQueue<T> {
    available: AtomicBool,
    capacity: Arc<Semaphore>,
    values: Mutex<QueueState<T>>,
    notifier: IrohNotifier,
    pressure: Pressure,
}

/// Reserves storage before a stream reader allocates its next bounded payload.
pub(super) struct Admission<'a, T> {
    queue: &'a IncomingQueue<T>,
    permit: OwnedSemaphorePermit,
}

impl<T> Admission<'_, T> {
    pub fn send(self, value: T) -> Result<()> {
        {
            let mut values = self
                .queue
                .values
                .lock()
                .map_err(|_| anyhow::anyhow!("Iroh peer input queue lock is poisoned"))?;
            ensure!(self.queue.is_available(), "Iroh peer is unavailable");
            values.records.push_back((value, self.permit));
        }
        self.queue
            .notifier
            .notify()
            .context("could not wake Weld for Iroh input")
    }
}

struct QueueState<T> {
    records: VecDeque<(T, OwnedSemaphorePermit)>,
    last_report: Instant,
    reported_admissions: u64,
}

#[derive(Default)]
struct Pressure {
    parked: AtomicUsize,
    admissions: AtomicU64,
    longest_completed_wait_us: AtomicU64,
}

struct Parked<'a> {
    pressure: &'a Pressure,
    started: Instant,
}

impl Drop for Parked<'_> {
    fn drop(&mut self) {
        let micros = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.pressure
            .longest_completed_wait_us
            .fetch_max(micros, Ordering::Relaxed);
        self.pressure.parked.fetch_sub(1, Ordering::Relaxed);
    }
}

impl<T> IncomingQueue<T> {
    pub fn new(notifier: IrohNotifier) -> Self {
        Self::with_capacity(notifier, QUEUE_CAPACITY)
    }

    pub fn with_capacity(notifier: IrohNotifier, capacity: usize) -> Self {
        Self {
            available: AtomicBool::new(true),
            capacity: Arc::new(Semaphore::new(capacity)),
            values: Mutex::new(QueueState {
                records: VecDeque::new(),
                last_report: Instant::now(),
                reported_admissions: 0,
            }),
            notifier,
            pressure: Pressure::default(),
        }
    }

    pub async fn push(&self, value: T) -> Result<()> {
        self.reserve().await?.send(value)
    }

    pub async fn reserve(&self) -> Result<Admission<'_, T>> {
        let permit = match self.capacity.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::Closed) => anyhow::bail!("Iroh peer is unavailable"),
            Err(TryAcquireError::NoPermits) => {
                self.pressure.parked.fetch_add(1, Ordering::Relaxed);
                self.pressure.admissions.fetch_add(1, Ordering::Relaxed);
                let _parked = Parked {
                    pressure: &self.pressure,
                    started: Instant::now(),
                };
                self.capacity
                    .clone()
                    .acquire_owned()
                    .await
                    .context("Iroh peer is unavailable")?
            }
        };
        Ok(Admission {
            queue: self,
            permit,
        })
    }

    pub fn drain(&self) -> Result<Vec<T>> {
        self.drain_matching(usize::MAX, |_| true)
    }

    pub fn drain_matching(&self, limit: usize, mut fits: impl FnMut(&T) -> bool) -> Result<Vec<T>> {
        let (records, report) = {
            let mut values = self
                .values
                .lock()
                .map_err(|_| anyhow::anyhow!("Iroh peer input queue lock is poisoned"))?;
            ensure!(
                !values.records.is_empty() || self.is_available(),
                "Iroh peer is unavailable"
            );
            let admissions = self.pressure.admissions.load(Ordering::Relaxed);
            let parked = self.pressure.parked.load(Ordering::Relaxed);
            let report = if (admissions != values.reported_admissions || parked != 0)
                && values.last_report.elapsed() >= Duration::from_secs(1)
            {
                values.last_report = Instant::now();
                values.reported_admissions = admissions;
                Some((
                    admissions,
                    parked,
                    self.pressure
                        .longest_completed_wait_us
                        .load(Ordering::Relaxed),
                ))
            } else {
                None
            };
            // This is the only owner that releases admitted records' permits.
            // Cancelled admission futures release their unqueued permits by RAII.
            let mut records = Vec::new();
            while records.len() < limit
                && values
                    .records
                    .front()
                    .is_some_and(|(record, _)| fits(record))
            {
                if let Some((record, _permit)) = values.records.pop_front() {
                    records.push(record);
                }
            }
            (records, report)
        };
        if let Some((parked_admissions, currently_parked, longest_completed_wait_us)) = report {
            tracing::debug!(target: "weld_network_diag", parked_admissions, currently_parked,
                longest_completed_wait_us, "Iroh incoming queue pressure");
        }
        Ok(records)
    }

    pub fn wake_if(&self, fits: impl FnOnce(&T) -> bool) -> Result<()> {
        let ready = self
            .values
            .lock()
            .map_err(|_| anyhow::anyhow!("Iroh peer input queue lock is poisoned"))?
            .records
            .front()
            .is_some_and(|(record, _)| fits(record));
        if ready {
            self.notifier.notify()?;
        }
        Ok(())
    }

    pub fn is_available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }

    pub fn fail(&self) -> bool {
        if self.available.swap(false, Ordering::AcqRel) {
            self.capacity.close();
            let _ = self.notifier.notify();
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_lite::future::poll_once;
    use std::{
        io,
        sync::{OnceLock, Weak},
    };

    use super::*;

    fn queue() -> IncomingQueue<usize> {
        let notifier = IrohNotifier::new(|| Ok(()));
        IncomingQueue::new(notifier)
    }

    #[tokio::test]
    async fn wake_runs_after_publication_without_holding_the_queue_lock() {
        let slot = Arc::new(OnceLock::<Weak<IncomingQueue<usize>>>::new());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let callback_slot = slot.clone();
        let callback_observed = observed.clone();
        let notifier = IrohNotifier::new(move || {
            let queue = callback_slot.get().and_then(Weak::upgrade).expect("queue");
            // Fail rather than hang if publication starts retaining this lock.
            let values = queue.values.try_lock().expect("queue unlocked during wake");
            let records = values.records.iter().map(|(record, _)| *record);
            callback_observed.lock().expect("observed").extend(records);
            Ok(())
        });
        let queue = Arc::new(IncomingQueue::with_capacity(notifier, 1));
        assert!(slot.set(Arc::downgrade(&queue)).is_ok());
        queue.push(7).await.expect("publish and wake");
        assert_eq!(*observed.lock().expect("observed"), [7]);
        assert_eq!(queue.drain().expect("drain"), [7]);
        assert_eq!(queue.capacity.available_permits(), 1);
    }

    #[tokio::test]
    async fn failed_wake_preserves_queued_data_and_budgeted_rearming() {
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = calls.clone();
        let queue = IncomingQueue::with_capacity(
            IrohNotifier::new(move || {
                callback_calls.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::other("test wake unavailable"))
            }),
            1,
        );
        let error = queue.push(7).await.expect_err("wake failure propagates");
        assert!(format!("{error:#}").contains("test wake unavailable"));
        assert_eq!(queue.capacity.available_permits(), 0);
        assert!(queue.wake_if(|_| false).is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(queue.wake_if(|_| true).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(queue.drain().expect("queued record survives"), [7]);
        assert_eq!(queue.capacity.available_permits(), 1);
        assert!(queue.fail());
        assert!(!queue.fail());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn reservation_precedes_payload_and_partial_drain_preserves_fifo() {
        let notifier = IrohNotifier::new(|| Ok(()));
        let media = IncomingQueue::with_capacity(notifier, 2);
        media
            .reserve()
            .await
            .expect("first body slot")
            .send(7)
            .expect("publish");
        media
            .reserve()
            .await
            .expect("second body slot")
            .send(3)
            .expect("publish");
        let mut third = Box::pin(media.reserve());
        assert!(poll_once(third.as_mut()).await.is_none());
        assert!(
            media
                .drain_matching(2, |size| *size <= 3)
                .expect("head does not fit")
                .is_empty()
        );
        assert!(poll_once(third.as_mut()).await.is_none());
        assert_eq!(
            media.drain_matching(1, |_| true).expect("one record"),
            vec![7]
        );
        poll_once(third.as_mut())
            .await
            .expect("body slot freed")
            .expect("reserve")
            .send(5)
            .expect("publish");
        assert_eq!(
            media.drain_matching(2, |_| true).expect("remaining order"),
            vec![3, 5]
        );
        assert_eq!(media.capacity.available_permits(), 2);
    }

    #[tokio::test]
    async fn saturation_waits_and_batch_drain_resumes_in_order() {
        let queue = queue();
        for value in 0..QUEUE_CAPACITY {
            queue.push(value).await.expect("admission");
        }
        let mut next = Box::pin(queue.push(QUEUE_CAPACITY));
        assert!(poll_once(next.as_mut()).await.is_none());
        assert!(queue.is_available());
        assert_eq!(queue.pressure.parked.load(Ordering::Relaxed), 1);
        // Opposite-direction work can run while ingress admission is parked.
        let (mut send, mut recv) = tokio::io::duplex(16);
        crate::framing::write_record(&mut send, &7_u8)
            .await
            .expect("write");
        assert_eq!(
            crate::framing::read_record::<_, u8>(&mut recv)
                .await
                .expect("read"),
            7
        );
        assert_eq!(
            queue.drain().expect("batch"),
            (0..QUEUE_CAPACITY).collect::<Vec<_>>()
        );
        assert!(poll_once(next.as_mut()).await.expect("resumed").is_ok());
        assert_eq!(queue.drain().expect("next batch"), vec![QUEUE_CAPACITY]);
        assert_eq!(queue.capacity.available_permits(), QUEUE_CAPACITY);
    }

    #[tokio::test]
    async fn cancelling_a_waiter_does_not_leak_capacity_or_pressure() {
        let queue = queue();
        for value in 0..QUEUE_CAPACITY {
            queue.push(value).await.expect("admission");
        }
        let mut next = Box::pin(queue.push(999));
        assert!(poll_once(next.as_mut()).await.is_none());
        drop(next);
        assert_eq!(queue.pressure.parked.load(Ordering::Relaxed), 0);
        queue.drain().expect("drain");
        assert_eq!(queue.capacity.available_permits(), QUEUE_CAPACITY);
    }

    #[tokio::test]
    async fn close_wakes_parked_admission_and_preserves_buffered_records() {
        let queue = queue();
        for value in 0..QUEUE_CAPACITY {
            queue.push(value).await.expect("admission");
        }
        let mut next = Box::pin(queue.push(999));
        assert!(poll_once(next.as_mut()).await.is_none());
        assert!(queue.fail());
        assert!(!queue.fail());
        assert!(
            poll_once(next.as_mut())
                .await
                .expect("closed waiter")
                .is_err()
        );
        assert_eq!(queue.drain().expect("last batch").len(), QUEUE_CAPACITY);
        assert!(queue.drain().is_err());
        assert!(queue.push(1).await.is_err());
    }
}
