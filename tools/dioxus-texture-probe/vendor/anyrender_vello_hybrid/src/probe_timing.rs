//! Probe-local wall timings around renderer calls on the render thread.
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
static NONBLOCKING: AtomicBool = AtomicBool::new(false);
pub fn set_probe_nonblocking_poll(enabled: bool) {
    NONBLOCKING.store(enabled, Ordering::Relaxed);
}
pub(crate) fn nonblocking() -> bool {
    NONBLOCKING.load(Ordering::Relaxed)
}

#[derive(Default)]
pub(crate) struct Sample {
    pub scene: Duration,
    pub acquire: Duration,
    pub encode: Duration,
    pub submit: Duration,
    pub present: Duration,
    pub wait: Duration,
}
pub(crate) struct Trace {
    since: Instant,
    frames: u64,
    sums: Sample,
}
impl Default for Trace {
    fn default() -> Self {
        Self {
            since: Instant::now(),
            frames: 0,
            sums: Sample::default(),
        }
    }
}
impl Trace {
    pub fn record(&mut self, sample: Sample) {
        self.frames += 1;
        self.sums.scene += sample.scene;
        self.sums.acquire += sample.acquire;
        self.sums.encode += sample.encode;
        self.sums.submit += sample.submit;
        self.sums.present += sample.present;
        self.sums.wait += sample.wait;
        if self.since.elapsed() >= Duration::from_secs(1) {
            eprintln!(
                "probe_render elapsed_ms={} frames={} scene_us={} acquire_us={} encode_us={} submit_us={} present_us={} wait_us={} nonblocking={}",
                self.since.elapsed().as_millis(),
                self.frames,
                self.sums.scene.as_micros(),
                self.sums.acquire.as_micros(),
                self.sums.encode.as_micros(),
                self.sums.submit.as_micros(),
                self.sums.present.as_micros(),
                self.sums.wait.as_micros(),
                nonblocking()
            );
            *self = Self::default();
        }
    }
}
