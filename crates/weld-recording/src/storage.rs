//! Anonymous packet storage written by a bounded worker, never by the host loop.

use crate::{FrameInfo, RecordedEvent};
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    os::unix::fs::FileExt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    thread,
};
use weld_media::{EncodedAccessUnit, EncodedFrameKind, VideoCodec};

const PENDING_PACKETS: usize = 8;

pub struct EncodedFrame {
    pub info: FrameInfo,
    pub unit: Packet,
}

/// Only codec information needed for inspection and remuxing is retained.
pub struct Packet {
    pub codec: VideoCodec,
    pub kind: EncodedFrameKind,
    payload: Payload,
}

enum Payload {
    Memory(Vec<u8>),
    Stored {
        file: Arc<File>,
        offset: u64,
        length: usize,
    },
}

impl From<EncodedAccessUnit> for Packet {
    fn from(unit: EncodedAccessUnit) -> Self {
        Self {
            codec: unit.codec,
            kind: unit.kind,
            payload: Payload::Memory(unit.payload),
        }
    }
}

impl Packet {
    pub fn bytes(&self) -> Result<Vec<u8>> {
        match &self.payload {
            Payload::Memory(bytes) => Ok(bytes.clone()),
            Payload::Stored {
                file,
                offset,
                length,
            } => {
                let mut bytes = vec![0; *length];
                file.read_exact_at(&mut bytes, *offset)?;
                Ok(bytes)
            }
        }
    }

    pub fn len(&self) -> usize {
        match &self.payload {
            Payload::Memory(bytes) => bytes.len(),
            Payload::Stored { length, .. } => *length,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Default)]
struct Progress {
    frames: Vec<EncodedFrame>,
    events: Vec<RecordedEvent>,
    error: Option<String>,
}

/// A cheap handle for copying the live timeline only when explicitly requested.
#[derive(Clone, Default)]
pub struct Timeline(Arc<Mutex<Progress>>);
impl Timeline {
    pub fn events(&self) -> Result<Vec<RecordedEvent>> {
        Ok(self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("recording storage unavailable"))?
            .events
            .clone())
    }

    pub fn frames(&self) -> Result<Vec<FrameInfo>> {
        let progress = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("recording storage unavailable"))?;
        Ok(progress
            .frames
            .iter()
            .map(|frame| frame.info.clone())
            .collect())
    }
}

pub(crate) struct Storage {
    sender: SyncSender<(FrameInfo, EncodedAccessUnit)>,
    progress: Arc<Mutex<Progress>>,
    cancelled: Arc<AtomicBool>,
    submitted: usize,
    event_count: u32,
}

impl Storage {
    pub fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self> {
        // Bound both queue length and individual payloads before admission.
        let (sender, receiver) = sync_channel::<(FrameInfo, EncodedAccessUnit)>(PENDING_PACKETS);
        let progress = Arc::new(Mutex::new(Progress::default()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_progress = progress.clone();
        let worker_cancelled = cancelled.clone();
        thread::Builder::new()
            .name("recording-storage".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let file =
                        Arc::new(tempfile::tempfile().context("creating recording storage")?);
                    let mut offset = 0;
                    for (info, unit) in receiver {
                        if worker_cancelled.load(Ordering::Acquire) {
                            break;
                        }
                        file.write_all_at(&unit.payload, offset)
                            .context("writing recording packet")?;
                        let length = unit.payload.len();
                        let frame = EncodedFrame {
                            info,
                            unit: Packet {
                                codec: unit.codec,
                                kind: unit.kind,
                                payload: Payload::Stored {
                                    file: file.clone(),
                                    offset,
                                    length,
                                },
                            },
                        };
                        offset += length as u64;
                        worker_progress
                            .lock()
                            .map_err(|_| anyhow::anyhow!("recording storage unavailable"))?
                            .frames
                            .push(frame);
                        wake();
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    if let Ok(mut progress) = worker_progress.lock() {
                        progress.error = Some(error.to_string());
                    }
                    wake();
                }
            })?;
        Ok(Self {
            sender,
            progress,
            cancelled,
            submitted: 0,
            event_count: 0,
        })
    }

    pub fn push(&mut self, mut info: FrameInfo, unit: EncodedAccessUnit) -> Result<()> {
        info.index = self.submitted as u32;
        self.sender
            .try_send((info, unit))
            .map_err(|_| anyhow::anyhow!("recording storage cannot keep up"))?;
        self.submitted += 1;
        Ok(())
    }

    pub fn is_idle(&self) -> Result<bool> {
        let progress = self
            .progress
            .lock()
            .map_err(|_| anyhow::anyhow!("recording storage unavailable"))?;
        ensure!(
            progress.error.is_none(),
            "{}",
            progress.error.as_deref().unwrap_or_default()
        );
        Ok(progress.frames.len() == self.submitted)
    }

    pub fn has_capacity(&self, reserved: usize) -> bool {
        self.progress.lock().ok().is_some_and(|progress| {
            self.submitted.saturating_sub(progress.frames.len()) + reserved < PENDING_PACKETS
        })
    }

    pub fn note_event(
        &mut self,
        index: u32,
        action: &serde_json::Value,
        time_us: u64,
        complete: bool,
        revision: Option<String>,
    ) -> Result<()> {
        ensure!(index < 64, "input event index exceeds the action budget");
        let mut progress = self
            .progress
            .lock()
            .map_err(|_| anyhow::anyhow!("recording storage unavailable"))?;
        if let Some(event) = progress
            .events
            .iter_mut()
            .find(|event| event.index == index)
        {
            event.time_us = time_us;
            event.complete = complete;
            event.revision = revision;
        } else {
            progress.events.push(RecordedEvent {
                index,
                action: action.clone(),
                first_time_us: time_us,
                time_us,
                complete,
                revision,
            });
            self.event_count += 1;
        }
        Ok(())
    }

    pub fn event_count(&self) -> u32 {
        self.event_count
    }

    pub fn timeline(&self) -> Timeline {
        Timeline(self.progress.clone())
    }

    pub fn frame_count(&self) -> usize {
        self.submitted
    }

    pub fn finish(&mut self) -> Result<Vec<EncodedFrame>> {
        ensure!(self.is_idle()?, "recording storage is still writing");
        let mut progress = self
            .progress
            .lock()
            .map_err(|_| anyhow::anyhow!("recording storage unavailable"))?;
        Ok(std::mem::take(&mut progress.frames))
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Condvar, mpsc},
        time::Duration,
    };
    use weld_media::{MediaFrameId, MediaStreamId, StreamGeneration};

    struct Closed(mpsc::Sender<()>);
    impl Drop for Closed {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    #[test]
    fn storage_backpressures_and_closes_files_after_the_last_reader() -> Result<()> {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = gate.clone();
        let (notify, notifications) = mpsc::channel();
        let (closed, stopped) = mpsc::channel();
        let closed = Closed(closed);
        let mut storage = Storage::new(Arc::new(move || {
            let _closed = &closed;
            let _ = notify.send(());
            let (lock, ready) = &*worker_gate;
            let guard = lock.lock().expect("gate");
            let _guard = ready
                .wait_while(guard, |released| !*released)
                .expect("release");
        }))?;
        let push = |storage: &mut Storage, index| {
            storage.push(
                FrameInfo {
                    index,
                    time_us: u64::from(index),
                    revision: index.to_string(),
                },
                EncodedAccessUnit {
                    frame: MediaFrameId::new(
                        MediaStreamId::new(1),
                        StreamGeneration::new(1),
                        u64::from(index),
                    ),
                    codec: VideoCodec::Av1,
                    kind: EncodedFrameKind::Keyframe,
                    timestamp_micros: u64::from(index),
                    payload: index.to_le_bytes().to_vec(),
                },
            )
        };
        push(&mut storage, 0)?;
        notifications.recv_timeout(Duration::from_secs(5))?;
        for index in 1..=PENDING_PACKETS as u32 {
            push(&mut storage, index)?;
        }
        assert!(!storage.has_capacity(0));
        assert!(!storage.is_idle()?);
        assert_eq!(storage.timeline().frames()?.len(), 1);
        *gate.0.lock().expect("gate") = true;
        gate.1.notify_one();
        for _ in 0..PENDING_PACKETS {
            notifications.recv_timeout(Duration::from_secs(5))?;
        }
        assert!(storage.is_idle()?);
        let frames = storage.finish()?;
        assert_eq!(frames.len(), PENDING_PACKETS + 1);
        let Payload::Stored { file, .. } = &frames[0].unit.payload else {
            anyhow::bail!("packet was not stored in a file");
        };
        let file = Arc::downgrade(file);
        drop(storage);
        stopped.recv_timeout(Duration::from_secs(5))?;
        assert!(file.upgrade().is_some(), "retained clip owns the file");
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(frame.unit.bytes()?, (index as u32).to_le_bytes());
        }
        drop(frames);
        assert!(
            file.upgrade().is_none(),
            "last reader closes temporary storage"
        );
        Ok(())
    }
}
