//! One finite fixture player. No Godot object crosses to codec/render threads.
//! The shared render adapter retains native images until GPU release fences
//! signal. Unrecoverable context loss quarantines bounded leases and prohibits
//! replay instead of risking buffer reuse.
mod render;
mod retirement;

use crate::{
    fixture,
    native::{self, Image, Progress},
};
use anyhow::{Context, Result, ensure};
use godot::{
    classes::{Engine, Object, RenderingServer},
    prelude::*,
};
use retirement::Retired;
use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
const FIXTURE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/panel-av1.ivf"));

#[derive(Default)]
struct Shared {
    cancelled: AtomicBool,
    queued: AtomicBool,
    latest: Mutex<Option<Image>>,
    retired: Mutex<Vec<Retired>>,
    target: Mutex<Option<native::Target>>,
    pending: Mutex<Option<Image>>,
    error: Mutex<Option<String>>,
    decoded: AtomicU64,
    presented: AtomicU64,
    replaced: AtomicU64,
    done: AtomicBool,
    decoder_ready: AtomicBool,
}
impl Shared {
    fn fail(&self, error: impl std::fmt::Display) {
        let mut slot = lock(&self.error);
        if slot.is_none() {
            *slot = Some(error.to_string());
        }
        self.cancelled.store(true, Ordering::Release);
    }
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // No guarded code calls user callbacks or waits for native work. On unwind,
    // slot ownership remains valid and can still be cancelled/drained safely.
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub struct Controller {
    generation: u64,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    material: Gd<Object>,
    stopped: bool,
}
impl Controller {
    pub fn start(texture: i64, material: Gd<Object>) -> Result<Self> {
        ensure!(
            !Engine::singleton().is_editor_hint(),
            "video playback is disabled inside the editor"
        );
        ensure!(
            material.is_class("ShaderMaterial"),
            "expected a ShaderMaterial"
        );
        let texture = u32::try_from(texture)?;
        ensure!(texture != 0, "external GL texture unavailable");
        let generation = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(Shared::default());
        render::queue(
            generation,
            Arc::clone(&shared),
            render::Operation::Open(texture),
        );
        RenderingServer::singleton().force_sync();
        if let Some(error) = lock(&shared.error).as_ref() {
            anyhow::bail!("{error}");
        }
        let mut controller = Self {
            generation,
            shared,
            material,
            worker: None,
            stopped: false,
        };
        let shared = Arc::clone(&controller.shared);
        controller.worker = Some(
            thread::Builder::new()
                .name("weld-video-fixture".into())
                .spawn(move || {
                    if let Err(error) = play(&shared)
                        && !shared.cancelled.load(Ordering::Acquire)
                    {
                        shared.fail(format!("decode: {error:#}"));
                    }
                    shared.done.store(true, Ordering::Release);
                })?,
        );
        Ok(controller)
    }
    pub fn tick(&mut self) {
        if let Err(error) = self.prepare() {
            self.shared.fail(format!("{error:#}"));
            self.stop();
        }
    }
    fn prepare(&mut self) -> Result<()> {
        if self.stopped {
            self.finished();
            return Ok(());
        }
        retirement::reap(&self.shared)?;
        if let Some(error) = lock(&self.shared.error).as_ref() {
            anyhow::bail!("{error}");
        }
        if self.shared.queued.load(Ordering::Acquire) || lock(&self.shared.retired).len() >= 2 {
            return Ok(());
        }
        let Some(image) = lock(&self.shared.latest).take() else {
            return Ok(());
        };
        let info = image.geometry();
        let [left, top, right, bottom] = info.crop;
        // Crop belongs to this exact pending image, never a later mailbox value.
        let crop = Vector4::new(
            (left as f32 + 0.5) / info.width as f32,
            (top as f32 + 0.5) / info.height as f32,
            (right as f32 - 0.5) / info.width as f32,
            (bottom as f32 - 0.5) / info.height as f32,
        );
        self.uniform("crop", crop.to_variant())?;
        self.uniform("has_frame", true.to_variant())?;
        *lock(&self.shared.pending) = Some(image);
        self.shared.queued.store(true, Ordering::Release);
        render::queue(
            self.generation,
            Arc::clone(&self.shared),
            render::Operation::Present,
        );
        Ok(())
    }
    pub fn stop(&mut self) {
        self.shared.cancelled.store(true, Ordering::Release);
        if let Err(error) = self
            .uniform("has_frame", false.to_variant())
            .and_then(|()| self.uniform("video", Variant::nil()))
        {
            self.shared
                .fail(format!("could not detach video sampler: {error:#}"));
            render::quarantine();
            return;
        }
        lock(&self.shared.latest).take();
        lock(&self.shared.pending).take();
        // Repeated stop retries quarantine only on the original EGL context.
        render::queue(
            self.generation,
            Arc::clone(&self.shared),
            render::Operation::Close,
        );
        RenderingServer::singleton().force_sync();
        if let Err(error) = retirement::finish(&self.shared) {
            self.shared
                .fail(format!("native video restart required: {error:#}"));
            render::quarantine();
        }
        self.stopped = true;
        self.finished();
    }
    fn uniform(&mut self, name: &str, value: Variant) -> Result<()> {
        self.material
            .try_call("set_shader_parameter", &[name.to_variant(), value])
            .map(|_| ())
            .map_err(|error| anyhow::anyhow!("material parameter {name}: {error}"))
    }
    pub fn finished(&mut self) -> bool {
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(worker) = self.worker.take()
                && worker.join().is_err()
            {
                self.shared.fail("decoder worker panicked");
            }
            lock(&self.shared.latest).take();
        }
        self.worker.is_none()
    }
    pub fn status(&self) -> String {
        if let Some(error) = lock(&self.shared.error).as_ref() {
            return error.clone();
        }
        let state = if self.stopped {
            "Stopped"
        } else if self.shared.done.load(Ordering::Acquire) {
            "Finished - replay available"
        } else if !self.shared.decoder_ready.load(Ordering::Acquire) {
            "Opening AV1 decoder"
        } else {
            "Playing AV1"
        };
        format!(
            "{state}: decoded {}, presented {}, superseded {}",
            self.shared.decoded.load(Ordering::Relaxed),
            self.shared.presented.load(Ordering::Relaxed),
            self.shared.replaced.load(Ordering::Relaxed)
        )
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.stop();
        // Final exit must not unload Rust/native code under a worker. A wedged
        // driver can delay this join; ordinary Stop only polls completion.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn play(shared: &Shared) -> Result<()> {
    let clip = fixture::parse(FIXTURE)?;
    let target = lock(&shared.target)
        .take()
        .context("native import target missing")?;
    let mut decoder = native::Decoder::new(&clip.config, target)?;
    shared.decoder_ready.store(true, Ordering::Release);
    let start = Instant::now();
    for frame in &clip.frames {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !shared.cancelled.load(Ordering::Acquire) {
            if start.elapsed().as_micros() >= u128::from(frame.timestamp)
                && decoder.try_send(frame.bytes, frame.timestamp)?
            {
                break;
            }
            drain(&mut decoder, shared)?;
            ensure!(Instant::now() < deadline, "fixture submission timed out");
            thread::sleep(Duration::from_millis(1));
        }
        if shared.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        drain(&mut decoder, shared)?;
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while !shared.cancelled.load(Ordering::Acquire) && !decoder.try_finish()? {
        drain(&mut decoder, shared)?;
        ensure!(
            Instant::now() < deadline,
            "fixture EOS submission timed out"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while !shared.cancelled.load(Ordering::Acquire) {
        if drain(&mut decoder, shared)? {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "fixture EOS drain timed out");
        thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}
fn drain(decoder: &mut native::Decoder, shared: &Shared) -> Result<bool> {
    loop {
        match decoder
            .receive(|| shared.cancelled.load(Ordering::Acquire))
            .context("receive image")?
        {
            Progress::Pending => return Ok(false),
            Progress::End => return Ok(true),
            Progress::Image(image) => {
                shared.decoded.fetch_add(1, Ordering::Relaxed);
                if shared.cancelled.load(Ordering::Acquire) {
                    return Ok(false);
                }
                if lock(&shared.latest).replace(image).is_some() {
                    shared.replaced.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}
