//! Opt-in, finite local-video experiment. No transport or production scheduling changes.
use super::{Shared, drain, lock, native};
use crate::fixture;
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    io::Read,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct Counters {
    pub selection_age_us: AtomicU64,
    pub selection_age_max_us: AtomicU64,
    pub submitted: AtomicU64,
    pub late: AtomicU64,
    pub ticks: AtomicU64,
    pub queued: AtomicU64,
    pub fences: AtomicU64,
    pub empty: AtomicU64,
    pub render_wait_us: AtomicU64,
    pub render_wait_max_us: AtomicU64,
    pub import_us: AtomicU64,
    pub import_max_us: AtomicU64,
}

pub(super) fn run(shared: &Shared, path: &Path, seconds: u64) -> Result<()> {
    ensure!(
        (1..=60).contains(&seconds),
        "stress duration must be 1..60 seconds"
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    let clip = fixture::parse(&bytes)?;
    ensure!(
        clip.frames.len() >= 2,
        "stress clip needs two or more frames"
    );
    // Fixtures start with an independently decodable frame. Replaying the
    // short GOP bounds disk/memory, not decoder work.
    ensure!(
        clip.scale == 1 && [30, 60, 90].contains(&clip.rate),
        "stress fixture must be 30, 60 or 90 Hz"
    );
    ensure!(
        clip.frames
            .iter()
            .enumerate()
            .all(|(index, frame)| frame.timestamp == index as u64 * 1_000_000 / clip.rate),
        "stress fixture has irregular timestamps"
    );
    let target = lock(&shared.target)
        .take()
        .context("native target missing")?;
    let mut decoder = native::Decoder::new(&clip.config, target)?;
    let stats = shared
        .diagnostic
        .as_ref()
        .context("stress counters missing")?;
    shared.decoder_ready.store(true, Ordering::Release);
    let start = Instant::now();
    let end = start + Duration::from_secs(seconds);
    let interval = Duration::from_nanos(1_000_000_000 / clip.rate);
    let mut due = start;
    let mut index = 0usize;
    while Instant::now() < end && !shared.session.cancelled.load(Ordering::Acquire) {
        let frame = &clip.frames[index % clip.frames.len()];
        if Instant::now() >= due
            && decoder.try_send(frame.bytes, index as u64 * 1_000_000 / clip.rate)?
        {
            stats.submitted.fetch_add(1, Ordering::Relaxed);
            let now = Instant::now();
            if now > due + interval {
                stats.late.fetch_add(1, Ordering::Relaxed);
            }
            due = next_due(due, now, interval);
            index += 1;
        }
        drain(&mut decoder, shared)?;
        thread::sleep(Duration::from_micros(500));
    }
    // Drain a bounded tail; cancellation still uses the normal provider cleanup.
    let end = Instant::now() + Duration::from_secs(3);
    let mut ended = false;
    while Instant::now() < end && !shared.session.cancelled.load(Ordering::Acquire) {
        if !ended {
            ended = decoder.try_finish()?;
        }
        if drain(&mut decoder, shared)? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(1));
    }
    ensure!(
        shared.session.cancelled.load(Ordering::Acquire),
        "stress drain timed out"
    );
    Ok(())
}

fn next_due(previous: Instant, now: Instant, interval: Duration) -> Instant {
    if now > previous + interval {
        now + interval
    } else {
        previous + interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slow_fixture_submission_never_accumulates_catchup_credits() {
        let start = Instant::now();
        let interval = Duration::from_nanos(1_000_000_000 / 90);
        assert_eq!(
            next_due(start, start + interval / 2, interval),
            start + interval
        );
        assert_eq!(
            next_due(start, start + interval * 5, interval),
            start + interval * 6
        );
    }
}
