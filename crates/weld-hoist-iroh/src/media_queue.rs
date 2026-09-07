//! Media queue accounting follows record ownership through the complete write.
//! The bounded metadata queue holds no payloads and never outlives their guards.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::Result;
use tokio::{
    io::AsyncWrite,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
};
use weld_hoist_encoded::{MediaSendCounters, MediaSendSnapshot};
use weld_hoist_protocol::MediaEnvelope;
use weld_media::EncodedAccessUnit;

use crate::framing::write_media;

type MediaPacket = MediaEnvelope<EncodedAccessUnit>;
const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct MediaSender {
    sender: mpsc::Sender<QueuedMedia>,
    accounting: Arc<Mutex<Accounting>>,
    bytes: Arc<Semaphore>,
    records: Arc<Semaphore>,
    maximum: usize,
}

struct PendingRecord {
    ticket: u64,
    bytes: u64,
    admitted_at: Instant,
    writing_at: Option<Instant>,
}

struct Accounting {
    valid: bool,
    next_ticket: Option<u64>,
    maximum: usize,
    pending: VecDeque<PendingRecord>,
    counters: MediaSendCounters,
}

pub(crate) struct QueuedMedia {
    packet: MediaPacket,
    ticket: SendTicket,
    _bytes: OwnedSemaphorePermit,
    _record: OwnedSemaphorePermit,
}

struct SendTicket {
    accounting: Arc<Mutex<Accounting>>,
    id: Option<u64>,
}

impl MediaSender {
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<QueuedMedia>) {
        let (sender, receiver) = mpsc::channel(capacity);
        // One record can be out of the channel while its reliable write blocks.
        let maximum = capacity.saturating_add(1);
        let accounting = Arc::new(Mutex::new(Accounting {
            valid: true,
            next_ticket: Some(1),
            maximum,
            pending: VecDeque::with_capacity(maximum),
            counters: MediaSendCounters::default(),
        }));
        (
            Self {
                sender,
                accounting,
                bytes: Arc::new(Semaphore::new(MAX_PENDING_BYTES)),
                records: Arc::new(Semaphore::new(maximum)),
                maximum,
            },
            receiver,
        )
    }

    pub fn try_send(
        &self,
        packet: MediaPacket,
    ) -> Result<(), mpsc::error::TrySendError<MediaPacket>> {
        // Rejected records never enter accounting. Preserve Full/Closed semantics.
        let permit = match self.sender.try_reserve() {
            Ok(permit) => permit,
            Err(mpsc::error::TrySendError::Full(())) => {
                return Err(mpsc::error::TrySendError::Full(packet));
            }
            Err(mpsc::error::TrySendError::Closed(())) => {
                return Err(mpsc::error::TrySendError::Closed(packet));
            }
        };
        let Ok(bytes) = u32::try_from(packet.access_unit.payload.len()) else {
            return Err(mpsc::error::TrySendError::Full(packet));
        };
        let Ok(byte_permit) = self.bytes.clone().try_acquire_many_owned(bytes) else {
            return Err(mpsc::error::TrySendError::Full(packet));
        };
        let Ok(record_permit) = self.records.clone().try_acquire_owned() else {
            return Err(mpsc::error::TrySendError::Full(packet));
        };
        let mut accounting = self.accounting.lock().ok();
        let id = accounting.as_mut().and_then(|state| {
            if !state.valid {
                return None;
            }
            let Some(id) = state.next_ticket else {
                state.valid = false;
                return None;
            };
            if state.pending.len() >= state.maximum {
                state.valid = false;
                return None;
            }
            state.next_ticket = id.checked_add(1);
            let bytes = u64::try_from(packet.access_unit.payload.len()).unwrap_or(u64::MAX);
            state.pending.push_back(PendingRecord {
                ticket: id,
                bytes,
                admitted_at: Instant::now(),
                writing_at: None,
            });
            state.counters.accepted_records = state.counters.accepted_records.saturating_add(1);
            state.counters.accepted_payload_bytes =
                state.counters.accepted_payload_bytes.saturating_add(bytes);
            Some(id)
        });
        // Publication is ordered with admission for cloned producers. A poisoned
        // lock instead sends unaccounted; snapshots remain unavailable thereafter.
        permit.send(QueuedMedia {
            packet,
            _bytes: byte_permit,
            _record: record_permit,
            ticket: SendTicket {
                accounting: self.accounting.clone(),
                id,
            },
        });
        drop(accounting);
        Ok(())
    }

    pub fn headroom(&self) -> bool {
        self.maximum
            .saturating_sub(self.records.available_permits())
            < 4
            && MAX_PENDING_BYTES.saturating_sub(self.bytes.available_permits()) < 8 * 1024 * 1024
    }

    pub fn snapshot(&self, now: Instant) -> Option<MediaSendSnapshot> {
        let state = self.accounting.lock().ok()?;
        if !state.valid {
            return None;
        }
        Some(MediaSendSnapshot {
            counters: state.counters,
            pending_records: state.pending.len(),
            pending_payload_bytes: state
                .pending
                .iter()
                .fold(0_u64, |total, record| total.saturating_add(record.bytes)),
            oldest_pending_age: state
                .pending
                .front()
                .map(|record| now.saturating_duration_since(record.admitted_at))
                .unwrap_or_default(),
            active_write_age: state
                .pending
                .iter()
                .find_map(|record| record.writing_at)
                .map(|start| now.saturating_duration_since(start)),
        })
    }
}

impl SendTicket {
    fn started(&mut self, now: Instant) {
        let Some(id) = self.id else {
            return;
        };
        let Ok(mut state) = self.accounting.lock() else {
            return;
        };
        let Some(record) = state.pending.iter_mut().find(|record| record.ticket == id) else {
            state.valid = false;
            return;
        };
        record.writing_at = Some(now);
        let wait = now.saturating_duration_since(record.admitted_at);
        state.counters.queue_wait.record(wait);
    }

    fn finish(&mut self, completed_at: Option<Instant>) {
        let Some(id) = self.id.take() else {
            return;
        };
        let Ok(mut state) = self.accounting.lock() else {
            return;
        };
        let Some(index) = state.pending.iter().position(|record| record.ticket == id) else {
            state.valid = false;
            return;
        };
        let Some(record) = state.pending.remove(index) else {
            state.valid = false;
            return;
        };
        if let Some(now) = completed_at {
            let Some(start) = record.writing_at else {
                state.valid = false;
                return;
            };
            state.counters.completed_records = state.counters.completed_records.saturating_add(1);
            state.counters.completed_payload_bytes = state
                .counters
                .completed_payload_bytes
                .saturating_add(record.bytes);
            state
                .counters
                .write_wall
                .record(now.saturating_duration_since(start));
        } else {
            state.counters.cancelled_records = state.counters.cancelled_records.saturating_add(1);
            state.counters.cancelled_payload_bytes = state
                .counters
                .cancelled_payload_bytes
                .saturating_add(record.bytes);
        }
    }
}

impl Drop for SendTicket {
    fn drop(&mut self) {
        self.finish(None);
    }
}

/// Runs after the binding's stream preamble. Cancelling a partially written
/// record is terminal for that stream; the caller must close the peer as before.
pub(crate) async fn write_media_queue<W: AsyncWrite + Unpin>(
    writer: &mut W,
    receiver: &mut mpsc::Receiver<QueuedMedia>,
    writable: impl Fn() -> Result<()>,
) -> Result<()> {
    while let Some(QueuedMedia {
        packet,
        mut ticket,
        _bytes,
        _record,
    }) = receiver.recv().await
    {
        ticket.started(Instant::now());
        write_media(writer, packet).await?;
        ticket.finish(Some(Instant::now()));
        drop((_bytes, _record));
        writable()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, time::Duration};

    use futures_lite::future::poll_once;
    use tokio::io::duplex;
    use weld_hoist_protocol::HoistSessionId;
    use weld_media::{EncodedFrameKind, MediaFrameId, MediaStreamId, StreamGeneration, VideoCodec};

    use super::*;
    use crate::framing::read_media;

    fn packet(bytes: usize) -> MediaPacket {
        MediaEnvelope {
            session: HoistSessionId::new(1),
            access_unit: EncodedAccessUnit {
                frame: MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(1), 1),
                codec: VideoCodec::Av1,
                kind: EncodedFrameKind::Keyframe,
                timestamp_micros: 0,
                payload: vec![7; bytes],
            },
        }
    }

    #[tokio::test]
    async fn blocked_write_stays_charged_and_cancellation_releases_only_owned_records() {
        let (sender, mut receiver) = MediaSender::channel(2);
        sender.try_send(packet(32)).expect("first");
        sender.try_send(packet(64)).expect("second");
        let (mut writer, _reader) = duplex(1);
        let mut job = Box::pin(write_media_queue(&mut writer, &mut receiver, || Ok(())));
        assert!(poll_once(job.as_mut()).await.is_none());
        let blocked = sender
            .snapshot(Instant::now())
            .expect("snapshot without logging");
        assert_eq!(blocked.pending_records, 2);
        assert_eq!(blocked.pending_payload_bytes, 96);
        assert!(blocked.active_write_age.is_some());
        assert_eq!(blocked.counters.queue_wait.samples, 1);
        assert_eq!(blocked.counters.completed_records, 0);
        drop(job);
        let cancelled = sender.snapshot(Instant::now()).expect("cancelled active");
        assert_eq!(cancelled.pending_records, 1);
        assert_eq!(cancelled.pending_payload_bytes, 64);
        assert_eq!(cancelled.counters.cancelled_payload_bytes, 32);
        assert!(cancelled.active_write_age.is_none());
        drop(receiver);
        let closed = sender.snapshot(Instant::now()).expect("closed queue");
        assert_eq!(closed.pending_payload_bytes, 0);
        assert_eq!(closed.pending_records, 0);
        assert_eq!(closed.counters.cancelled_records, 2);
        assert_eq!(closed.counters.cancelled_payload_bytes, 96);
        assert_eq!(closed.counters.write_wall.samples, 0);
    }

    #[tokio::test]
    async fn full_framed_write_completes_once_without_cloning_payload() {
        let (sender, mut receiver) = MediaSender::channel(1);
        sender.try_send(packet(64)).expect("admit");
        receiver.close();
        let (mut writer, mut reader) = duplex(8);
        let wakes = Cell::new(0);
        let (sent, received) = tokio::join!(
            write_media_queue(&mut writer, &mut receiver, || {
                assert_eq!(sender.records.available_permits(), sender.maximum);
                assert_eq!(sender.bytes.available_permits(), MAX_PENDING_BYTES);
                wakes.set(wakes.get() + 1);
                Ok(())
            }),
            read_media(&mut reader)
        );
        sent.expect("write");
        assert_eq!(wakes.get(), 1);
        assert_eq!(received.expect("read").access_unit.payload, vec![7; 64]);
        let snapshot = sender.snapshot(Instant::now()).expect("completed");
        assert_eq!(snapshot.pending_records, 0);
        assert_eq!(snapshot.counters.accepted_records, 1);
        assert_eq!(snapshot.counters.completed_records, 1);
        assert_eq!(snapshot.counters.completed_payload_bytes, 64);
        assert_eq!(snapshot.counters.cancelled_records, 0);
        assert_eq!(snapshot.counters.write_wall.samples, 1);
    }

    #[tokio::test]
    async fn failed_write_is_cancelled_not_completed() {
        let (sender, mut receiver) = MediaSender::channel(1);
        sender.try_send(packet(10)).expect("admit");
        let (mut writer, reader) = duplex(1);
        drop(reader);
        assert!(
            write_media_queue(&mut writer, &mut receiver, || Ok(()))
                .await
                .is_err()
        );
        let snapshot = sender.snapshot(Instant::now()).expect("failed");
        assert_eq!(snapshot.pending_records, 0);
        assert_eq!(snapshot.counters.cancelled_records, 1);
        assert_eq!(snapshot.counters.completed_records, 0);
    }

    #[tokio::test]
    async fn admission_bound_and_full_closed_errors_are_preserved() {
        let (sender, mut receiver) = MediaSender::channel(1);
        sender.try_send(packet(10)).expect("admit");
        assert_eq!(
            sender.try_send(packet(10)).expect_err("full").to_string(),
            mpsc::error::TrySendError::Full(()).to_string()
        );
        let mut active = receiver.recv().await.expect("active record");
        sender
            .clone()
            .try_send(packet(20))
            .expect("queue while writing");
        let start = Instant::now();
        sender
            .accounting
            .lock()
            .expect("accounting")
            .pending
            .front_mut()
            .expect("entry")
            .admitted_at = start;
        active.ticket.started(start + Duration::from_millis(10));
        let snapshot = sender
            .snapshot(start + Duration::from_millis(30))
            .expect("bounded");
        assert_eq!(snapshot.pending_records, 2);
        assert_eq!(snapshot.pending_payload_bytes, 30);
        assert_eq!(snapshot.oldest_pending_age, Duration::from_millis(30));
        assert_eq!(snapshot.active_write_age, Some(Duration::from_millis(20)));
        assert_eq!(
            snapshot.counters.queue_wait.total,
            Duration::from_millis(10)
        );
        drop(active);
        drop(receiver);
        assert_eq!(
            sender.try_send(packet(10)).expect_err("closed").to_string(),
            mpsc::error::TrySendError::Closed(()).to_string()
        );
        assert_eq!(
            sender
                .snapshot(Instant::now())
                .expect("closed")
                .counters
                .accepted_records,
            2
        );
    }

    #[tokio::test]
    async fn byte_admission_returns_the_owned_packet_and_ignores_broken_diagnostics() {
        let (sender, receiver) = MediaSender::channel(8);
        sender.accounting.lock().expect("accounting").valid = false;
        // Hold capacity without allocating large synthetic frames.
        let held = sender
            .bytes
            .clone()
            .try_acquire_many_owned((MAX_PENDING_BYTES - 1) as u32)
            .expect("reserve bytes");
        let packet = packet(2);
        let pointer = packet.access_unit.payload.as_ptr();
        let rejected = match sender.try_send(packet) {
            Err(mpsc::error::TrySendError::Full(packet)) => packet,
            _ => panic!("byte bound must return Busy ownership"),
        };
        assert_eq!(rejected.access_unit.payload.as_ptr(), pointer);
        assert!(!sender.headroom());
        drop(held);
        assert!(sender.headroom());
        sender.try_send(rejected).expect("resume");
        assert_eq!(sender.bytes.available_permits(), MAX_PENDING_BYTES - 2);
        drop(receiver);
        assert_eq!(sender.bytes.available_permits(), MAX_PENDING_BYTES);
    }

    #[tokio::test]
    async fn broken_measurements_do_not_break_media_or_other_connections() {
        let (sender, mut receiver) = MediaSender::channel(2);
        let (other, _other_receiver) = MediaSender::channel(1);
        sender.accounting.lock().expect("accounting").next_ticket = None;
        sender.try_send(packet(10)).expect("unaccounted send");
        assert!(sender.snapshot(Instant::now()).is_none());
        assert!(receiver.recv().await.is_some());
        assert_eq!(
            other
                .snapshot(Instant::now())
                .expect("independent peer")
                .counters,
            MediaSendCounters::default()
        );
        let accounting = sender.accounting.clone();
        let _ = std::panic::catch_unwind(move || {
            let _guard = accounting.lock().expect("lock");
            panic!("poison only measurement state");
        });
        sender
            .try_send(packet(10))
            .expect("poisoned send still works");
        assert!(receiver.recv().await.is_some());
        assert!(sender.snapshot(Instant::now()).is_none());
    }
}
