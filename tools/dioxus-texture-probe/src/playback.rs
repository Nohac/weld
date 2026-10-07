//! Bounded fixture producer using the same native decoder as Weld Mobile.
use anyhow::{Result, ensure};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
use weld_media_android::{AndroidDecoder, AndroidImage, AndroidImageTarget, DecodeProgress};

pub struct Playback {
    pub frames: Receiver<Result<AndroidImage, String>>,
    stop: Arc<AtomicBool>,
}
impl Playback {
    pub fn start() -> Result<Self> {
        let (send, frames) = mpsc::sync_channel(2);
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = stop.clone();
        thread::Builder::new()
            .name("blitz-probe-decoder".into())
            .spawn(move || {
                if let Err(error) = decode(&send, &cancelled)
                    && !cancelled.load(Ordering::Acquire)
                {
                    log::error!("decode failed: {error:#}");
                    let _ = send.send(Err(format!("{error:#}")));
                }
            })?;
        Ok(Self { frames, stop })
    }
}
impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Dropping the receiver also wakes a producer parked on its bounded send.
    }
}

fn decode(send: &SyncSender<Result<AndroidImage, String>>, stop: &AtomicBool) -> Result<()> {
    let clip = crate::fixture::parse(include_bytes!(concat!(env!("OUT_DIR"), "/panel-av1.ivf")))?;
    let (width, height) = clip.config.extent();
    let target = AndroidImageTarget::new(width, height, 6)?;
    let mut decoder = AndroidDecoder::new(&clip.config, target)?;
    let mut submitted = 0;
    let mut received = 0;
    let mut finished = false;
    let mut progress = Instant::now();
    log::info!(
        "AV1 native decoder ready: {width}x{height}, {} frames at {}/{} Hz",
        clip.frames.len(),
        clip.rate,
        clip.scale
    );
    while !stop.load(Ordering::Acquire) {
        if let Some(frame) = clip.frames.get(submitted) {
            if decoder.try_send(frame.bytes, frame.timestamp)? {
                submitted += 1;
                progress = Instant::now();
            }
        } else if !finished {
            finished = decoder.try_finish()?;
        }
        match decoder.receive(|| stop.load(Ordering::Acquire))? {
            DecodeProgress::Image(image) => {
                if send.send(Ok(image)).is_err() {
                    return Ok(());
                }
                received += 1;
                progress = Instant::now();
            }
            DecodeProgress::End => {
                ensure!(
                    received == clip.frames.len(),
                    "decoded {received}/{} frames",
                    clip.frames.len()
                );
                log::info!("AV1 decode completed: {received} frames");
                return Ok(());
            }
            DecodeProgress::Pending => {
                ensure!(
                    progress.elapsed() < Duration::from_secs(5),
                    "decoder made no progress"
                );
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
    Ok(())
}
