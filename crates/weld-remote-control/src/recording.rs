//! Session-owned recording leases retain their original occupant's View grant.

use super::{GrantState, RemoteControlClient, ViewPermission};
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashMap,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use weld_recording::{Clip, FrameOptions, Image, Info, SheetOptions, Status, Video};

const IDLE_LEASE: Duration = Duration::from_secs(120);
const MAX_RECORDINGS: usize = 4;

struct Entry {
    permission: ViewPermission,
    live: Arc<AtomicBool>,
    expires: Instant,
    info: Info,
    clip: Option<Arc<Clip>>,
}
impl Drop for Entry {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}

#[derive(Clone, Default)]
pub(super) struct Archive(Arc<Mutex<HashMap<String, Entry>>>, Arc<Condvar>);
impl Archive {
    pub fn ensure_capacity(&self) -> Result<()> {
        self.cleanup(Instant::now());
        ensure!(
            self.0
                .lock()
                .map_err(|_| anyhow::anyhow!("recording archive unavailable"))?
                .len()
                < MAX_RECORDINGS,
            "recording capacity reached; release an existing recording"
        );
        Ok(())
    }
    pub fn insert(&self, grant: Arc<GrantState>, info: Info) -> Result<()> {
        let mut entries = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("recording archive unavailable"))?;
        ensure!(entries.len() < MAX_RECORDINGS, "recording capacity reached");
        entries.insert(
            info.id.clone(),
            Entry {
                permission: ViewPermission(grant),
                live: Arc::new(AtomicBool::new(true)),
                expires: Instant::now() + IDLE_LEASE,
                info,
                clip: None,
            },
        );
        Ok(())
    }
    pub fn active(&self, id: &str) -> bool {
        self.0.lock().ok().is_some_and(|entries| {
            entries.get(id).is_some_and(|entry| {
                entry.permission.validate().is_ok() && entry.expires > Instant::now()
            })
        })
    }
    pub fn update(&self, info: Info, clip: Option<Clip>) {
        if let Ok(mut entries) = self.0.lock()
            && let Some(entry) = entries.get_mut(&info.id)
        {
            entry.info = info;
            if let Some(clip) = clip {
                entry.clip = Some(Arc::new(clip));
                self.1.notify_all();
            }
        }
    }
    pub fn fail(&self, id: &str, error: String) {
        if let Ok(mut entries) = self.0.lock()
            && let Some(entry) = entries.get_mut(id)
        {
            entry.info.status = Status::Failed;
            entry.info.error = Some(error);
            self.1.notify_all();
        }
    }
    pub fn cleanup(&self, now: Instant) {
        if let Ok(mut entries) = self.0.lock() {
            let previous = entries.len();
            entries.retain(|_, entry| entry.expires > now && entry.permission.validate().is_ok());
            if previous != entries.len() {
                self.1.notify_all();
            }
        }
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        self.0
            .lock()
            .ok()?
            .values()
            .map(|entry| entry.expires)
            .min()
    }
    fn read<T>(&self, id: &str, read: impl FnOnce(&Entry) -> Result<T>) -> Result<T> {
        let mut entries = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("recording archive unavailable"))?;
        let entry = entries
            .get_mut(id)
            .context("recording unavailable or expired")?;
        ensure!(entry.expires > Instant::now(), "recording expired");
        entry.permission.validate()?;
        entry.expires = Instant::now() + IDLE_LEASE;
        read(entry)
    }
}

impl RemoteControlClient {
    pub fn wait_recording(&self, id: &str, timeout: Duration) -> Result<Info> {
        let deadline = Instant::now() + timeout;
        let mut entries = self
            .recordings
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("recording archive unavailable"))?;
        loop {
            let entry = entries
                .get_mut(id)
                .context("recording released or expired")?;
            entry.permission.validate()?;
            ensure!(entry.expires > Instant::now(), "recording expired");
            entry.expires = Instant::now() + IDLE_LEASE;
            if !matches!(entry.info.status, Status::Recording) {
                return Ok(entry.info.clone());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "recording wait timed out; inspect its ID for status"
            );
            (entries, _) = self
                .recordings
                .1
                .wait_timeout(entries, remaining)
                .map_err(|_| anyhow::anyhow!("recording archive unavailable"))?;
        }
    }
    pub fn recording_info(&self, id: &str) -> Result<Info> {
        self.recordings.read(id, |entry| Ok(entry.info.clone()))
    }
    fn inspect_recording<T>(
        &self,
        id: &str,
        inspect: impl FnOnce(&Clip, &dyn Fn() -> Result<()>) -> Result<T>,
    ) -> Result<T> {
        let (clip, permission, live) = self.recordings.read(id, |entry| {
            Ok((
                entry.clip.clone().context("recording is not complete")?,
                entry.permission.clone(),
                entry.live.clone(),
            ))
        })?;
        let validate = || {
            ensure!(
                live.load(Ordering::Acquire),
                "recording released or expired"
            );
            permission.validate()
        };
        validate()?;
        let result = inspect(&clip, &validate)?;
        validate()?;
        Ok(result)
    }
    pub fn recording_frame(&self, id: &str, options: FrameOptions) -> Result<Image> {
        self.inspect_recording(id, |clip, validate| {
            weld_recording::frame(clip, options, validate)
        })
    }
    pub fn recording_sheet(&self, id: &str, options: SheetOptions) -> Result<Image> {
        self.inspect_recording(id, |clip, validate| {
            weld_recording::sheet(clip, options, validate)
        })
    }
    pub fn recording_video(&self, id: &str) -> Result<Video> {
        self.inspect_recording(id, |clip, validate| weld_recording::video(clip, validate))
    }
    pub fn recording_release(&self, id: &str) -> Result<()> {
        self.recordings
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("recording archive unavailable"))?
            .remove(id)
            .context("recording unavailable")?;
        self.recordings.1.notify_all();
        (self.wake)();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Capabilities, ControlRegistry};
    use std::sync::{atomic::AtomicU64, mpsc};
    use weld_access::AccessAuthority;
    use weld_client::{ClientId, ClientSourceId, ClientSurfaceId};
    use weld_recording::{Codec, Region};

    #[test]
    fn idle_expiry_release_and_revocation_invalidate_readers_and_free_capacity() {
        let archive = Archive::default();
        let authority = AccessAuthority::new(|| {});
        let grant = Arc::new(GrantState {
            id: "control".into(),
            surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 1), 1),
            permit: authority.open("recording".into()).expect("permit"),
            capabilities: Capabilities::ALL,
            input_epoch: AtomicU64::new(0),
            input_epoch_applied: AtomicU64::new(0),
            remote_keyboard_target: AtomicBool::new(false),
            viewport: Mutex::new(None),
        });
        let info = Info {
            id: "clip".into(),
            status: Status::Complete,
            duration_ms: 500,
            codec: Codec::Av1,
            region: Region {
                x: 0.,
                y: 0.,
                width: 10.,
                height: 10.,
            },
            width: 10,
            height: 10,
            submitted_frames: 0,
            dropped_frames: Some(0),
            frame_count: 0,
            frames: vec![],
            bytes: 0,
            error: None,
            expires_after_idle_ms: 120_000,
        };
        archive.insert(grant.clone(), info.clone()).expect("insert");
        let live = archive
            .read("clip", |entry| Ok(entry.live.clone()))
            .expect("reader");
        archive.cleanup(Instant::now() + IDLE_LEASE + Duration::from_secs(1));
        assert!(!live.load(Ordering::Acquire));
        assert!(archive.read("clip", |_| Ok(())).is_err());
        archive
            .insert(grant.clone(), info.clone())
            .expect("reuse capacity");
        let before = archive.0.lock().expect("archive")["clip"].expires;
        archive.read("clip", |_| Ok(())).expect("renew");
        assert!(archive.0.lock().expect("archive")["clip"].expires >= before);
        let client = RemoteControlClient {
            sender: mpsc::sync_channel(1).0,
            grants: Arc::new(Mutex::new(ControlRegistry::default())),
            wake: Arc::new(|| {}),
            recordings: archive.clone(),
        };
        let live = archive
            .read("clip", |entry| Ok(entry.live.clone()))
            .expect("reader");
        client.recording_release("clip").expect("release");
        assert!(!live.load(Ordering::Acquire));
        archive.insert(grant.clone(), info).expect("insert again");
        grant.permit.end();
        assert!(client.recording_info("clip").is_err());
        archive.cleanup(Instant::now());
        assert!(archive.0.lock().expect("archive").is_empty());
    }
}
