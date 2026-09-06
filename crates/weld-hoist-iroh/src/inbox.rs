//! Bounded async admission into the compositor's synchronously drained inbox.
//! Control and media share destination capacity; draining never waits on a codec.

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
use weld_core::host::ClientRuntimeNotifier;

use crate::peer::QUEUE_CAPACITY;

pub(super) struct IncomingQueue<T> {
    available: AtomicBool,
    capacity: Arc<Semaphore>,
    values: Mutex<QueueState<T>>,
    notifier: ClientRuntimeNotifier,
    pressure: Pressure,
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
    pub fn new(notifier: ClientRuntimeNotifier) -> Self {
        Self {
            available: AtomicBool::new(true),
            capacity: Arc::new(Semaphore::new(QUEUE_CAPACITY)),
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
        {
            let mut values = self
                .values
                .lock()
                .map_err(|_| anyhow::anyhow!("Iroh peer input queue lock is poisoned"))?;
            ensure!(self.is_available(), "Iroh peer is unavailable");
            values.records.push_back((value, permit));
        }
        // Keep the level-triggered eventfd wake for every admission. A parked
        // reader must not depend on future traffic to repair a missed host wake.
        self.notifier
            .notify()
            .context("could not wake Weld for Iroh input")
    }

    pub fn drain(&self) -> Result<Vec<T>> {
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
            let records = values
                .records
                .drain(..)
                .map(|(record, _permit)| record)
                .collect();
            (records, report)
        };
        if let Some((parked_admissions, currently_parked, longest_completed_wait_us)) = report {
            tracing::debug!(target: "weld_network_diag", parked_admissions, currently_parked,
                longest_completed_wait_us, "Iroh incoming queue pressure");
        }
        Ok(records)
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
    use weld_core::host::client_runtime_notifier;

    use super::*;

    fn queue() -> IncomingQueue<usize> {
        let (notifier, _wake) = client_runtime_notifier().expect("notifier");
        IncomingQueue::new(notifier)
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
