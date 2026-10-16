//! Demand-driven preview cache shared by session bindings.
#[cfg(test)]
mod tests;
use anyhow::Result;
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
pub use weld_hoist_protocol::thumbnail::{
    ThumbnailFreshness, ThumbnailImageHeader, ThumbnailReply, ThumbnailRequest, ThumbnailRevision,
    ThumbnailStatus, ThumbnailTarget,
};
const CACHE_ENTRIES: usize = 32;
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// Encoded PNG storage shared across clients. Consumers must bound image decoding
/// to the advertised dimensions and verify the decoded extent matches the header.
#[derive(Clone, Debug)]
pub struct ThumbnailImage {
    header: ThumbnailImageHeader,
    png: Arc<[u8]>,
}
impl ThumbnailImage {
    pub fn new(width: u32, height: u32, png: Vec<u8>) -> Result<Self> {
        let header = ThumbnailImageHeader {
            width,
            height,
            png_bytes: u32::try_from(png.len())?,
        };
        header.validate().map_err(anyhow::Error::msg)?;
        Ok(Self {
            header,
            png: png.into(),
        })
    }
    pub fn header(&self) -> ThumbnailImageHeader {
        self.header
    }
    pub fn png(&self) -> &[u8] {
        &self.png
    }
}

#[derive(Clone, Debug)]
pub struct ThumbnailResponse {
    pub reply: ThumbnailReply,
    pub image: Option<ThumbnailImage>,
}
impl ThumbnailResponse {
    pub(super) fn status(target: ThumbnailTarget, status: ThumbnailStatus) -> Self {
        Self {
            reply: ThumbnailReply { target, status },
            image: None,
        }
    }
}

/// One capture ticket. Completion is accepted only while this ticket is current.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThumbnailCapture {
    pub target: ThumbnailTarget,
    revision: ThumbnailRevision,
}
struct Snapshot {
    revision: ThumbnailRevision,
    image: ThumbnailImage,
    captured: Instant,
}
struct Entry {
    snapshot: Option<Snapshot>,
    dirty: bool,
    requested: bool,
    last_attempt: Option<Instant>,
    last_used: Instant,
}
#[derive(Default)]
pub(super) struct Cache {
    entries: BTreeMap<ThumbnailTarget, Entry>,
    active: Option<(ThumbnailCapture, Instant)>,
    next: u64,
}
impl Cache {
    pub(super) fn retain(&mut self, mut visible: impl FnMut(ThumbnailTarget) -> bool) {
        self.entries.retain(|target, _| visible(*target));
        if self
            .active
            .is_some_and(|(capture, _)| !visible(capture.target))
        {
            self.active = None;
        }
    }
    pub(super) fn request(&mut self, request: ThumbnailRequest, now: Instant) -> ThumbnailResponse {
        if !self.entries.contains_key(&request.target) && self.entries.len() == CACHE_ENTRIES {
            let oldest = self
                .entries
                .iter()
                .filter(|(target, _)| {
                    self.active
                        .is_none_or(|(capture, _)| capture.target != **target)
                })
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(target, _)| *target);
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        let entry = self.entries.entry(request.target).or_insert(Entry {
            snapshot: None,
            dirty: true,
            requested: false,
            last_attempt: None,
            last_used: now,
        });
        entry.last_used = now;
        entry.requested |= entry.dirty
            || (entry.snapshot.is_none()
                && self
                    .active
                    .is_none_or(|(capture, _)| capture.target != request.target));
        let Some(snapshot) = &entry.snapshot else {
            return ThumbnailResponse::status(request.target, ThumbnailStatus::Pending);
        };
        let freshness = ThumbnailFreshness {
            age_ms: u64::try_from(now.saturating_duration_since(snapshot.captured).as_millis())
                .unwrap_or(u64::MAX),
            stale: entry.dirty
                || self
                    .active
                    .is_some_and(|(capture, _)| capture.target == request.target),
        };
        if request.if_revision == Some(snapshot.revision) {
            ThumbnailResponse::status(
                request.target,
                ThumbnailStatus::Unchanged {
                    revision: snapshot.revision,
                    freshness,
                },
            )
        } else {
            ThumbnailResponse {
                reply: ThumbnailReply {
                    target: request.target,
                    status: ThumbnailStatus::Image {
                        revision: snapshot.revision,
                        freshness,
                        image: snapshot.image.header,
                    },
                },
                image: Some(snapshot.image.clone()),
            }
        }
    }
    pub(super) fn take(&mut self, now: Instant) -> Option<ThumbnailCapture> {
        if let Some((capture, started)) = self.active {
            if now.saturating_duration_since(started) < CAPTURE_TIMEOUT {
                return None;
            }
            self.active = None;
            if let Some(entry) = self.entries.get_mut(&capture.target) {
                entry.dirty = true;
                entry.requested = true;
            }
        }
        let (&target, entry) = self
            .entries
            .iter_mut()
            .filter(|(_, entry)| {
                entry.requested
                    && entry
                        .last_attempt
                        .is_none_or(|last| now.saturating_duration_since(last) >= REFRESH_INTERVAL)
            })
            .min_by_key(|(_, entry)| entry.last_attempt)?;
        self.next = self.next.checked_add(1)?;
        let capture = ThumbnailCapture {
            target,
            revision: ThumbnailRevision(self.next),
        };
        entry.requested = false;
        entry.dirty = false;
        entry.last_attempt = Some(now);
        self.active = Some((capture, now));
        Some(capture)
    }
    pub(super) fn complete(
        &mut self,
        capture: ThumbnailCapture,
        image: Option<ThumbnailImage>,
        now: Instant,
    ) -> bool {
        if self.active.is_none_or(|(active, started)| {
            active != capture || now.saturating_duration_since(started) >= CAPTURE_TIMEOUT
        }) {
            return false;
        }
        self.active = None;
        let Some(entry) = self.entries.get_mut(&capture.target) else {
            return false;
        };
        if let Some(image) = image {
            entry.snapshot = Some(Snapshot {
                revision: capture.revision,
                image,
                captured: now,
            });
            entry.requested &= entry.dirty;
        } else {
            entry.dirty = true;
        }
        true
    }
}

impl Cache {
    pub(super) fn invalidate(&mut self, target: ThumbnailTarget) {
        if let Some(entry) = self.entries.get_mut(&target) {
            entry.dirty = true;
        }
    }
}
