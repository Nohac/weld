//! One GPU presentation session, fed by a fixture or the shared hoist receiver.
//! No Godot object crosses to codec/network/render threads.
//! The shared render adapter retains native images until GPU release fences
//! signal. Unrecoverable context loss quarantines bounded leases and prohibits
//! replay instead of risking buffer reuse.
mod frame;
mod input;
mod receiver;
mod receiver_decode;
mod render;
mod retirement;

use frame::Frame;

use crate::{
    fixture,
    native::{self, Progress},
};
use anyhow::{Context, Result, ensure};
use godot::{
    classes::{Engine, Object, RenderingServer},
    prelude::*,
};
use retirement::Retired;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use weld_client::{
    ClientCursor, InputPosition, KeyboardKeyState, PresentationRate, SurfaceContentView,
};

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
const FIXTURE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/panel-av1.ivf"));

pub enum Source {
    Fixture {
        single_frame: bool,
    },
    Iroh {
        directory: PathBuf,
        rate: PresentationRate,
    },
}

enum PresentationUpdate {
    Frame {
        frame: Frame,
        view: Option<SurfaceContentView>,
        input: Option<input::Target>,
    },
    View(SurfaceContentView, input::Target),
    Clear,
}

#[derive(Default)]
struct Shared {
    input: Mutex<input::InputState>,
    cancelled: AtomicBool,
    queued: AtomicBool,
    latest: Mutex<Option<PresentationUpdate>>,
    retired: Mutex<Vec<Retired>>,
    target: Mutex<Option<native::Target>>,
    pending: Mutex<Option<Frame>>,
    error: Mutex<Option<String>>,
    message: Mutex<Option<String>>,
    decoded: AtomicU64,
    presented: AtomicU64,
    replaced: AtomicU64,
    done: AtomicBool,
    decoder_ready: AtomicBool,
}
impl Shared {
    fn publish(&self, frame: Frame, view: Option<SurfaceContentView>) {
        self.publish_input(frame, view, None);
    }
    fn publish_input(
        &self,
        frame: Frame,
        view: Option<SurfaceContentView>,
        input: Option<input::Target>,
    ) {
        if self.cancelled.load(Ordering::Acquire) {
            return;
        }
        self.decoded.fetch_add(1, Ordering::Relaxed);
        if matches!(
            lock(&self.latest).replace(PresentationUpdate::Frame { frame, view, input }),
            Some(PresentationUpdate::Frame { .. })
        ) {
            self.replaced.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn set_view(&self, view: SurfaceContentView, input: input::Target) {
        let mut latest = lock(&self.latest);
        match latest.as_mut() {
            Some(PresentationUpdate::Frame {
                view: current,
                input: current_input,
                ..
            }) => {
                *current = Some(view);
                *current_input = Some(input);
            }
            Some(PresentationUpdate::Clear) => {}
            _ => *latest = Some(PresentationUpdate::View(view, input)),
        }
    }
    fn clear(&self) {
        lock(&self.input).invalidate();
        *lock(&self.latest) = Some(PresentationUpdate::Clear);
    }
    fn message(&self, message: impl AsRef<str>) {
        let mut current = lock(&self.message);
        if current.as_deref() != Some(message.as_ref()) {
            *current = Some(message.as_ref().to_owned());
        }
    }
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
    input_target: Option<input::Target>,
    generation: u64,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    material: Gd<Object>,
    _texture: Gd<Object>,
    current: Option<(native::Geometry, [u32; 2])>,
    aspect: f32,
    stopped: bool,
}
impl Controller {
    pub fn start(mut texture: Gd<Object>, material: Gd<Object>, source: Source) -> Result<Self> {
        ensure!(
            !Engine::singleton().is_editor_hint(),
            "video playback is disabled inside the editor"
        );
        ensure!(
            material.is_class("ShaderMaterial"),
            "expected a ShaderMaterial"
        );
        ensure!(
            texture.is_class("ExternalTexture"),
            "expected an ExternalTexture"
        );
        let texture_id = texture
            .try_call("get_external_texture_id", &[])
            .map_err(|error| anyhow::anyhow!("external texture: {error}"))?
            .try_to::<i64>()
            .map_err(|error| anyhow::anyhow!("invalid external texture ID: {error}"))?;
        let texture_id = u32::try_from(texture_id)?;
        ensure!(texture_id != 0, "external GL texture unavailable");
        let generation = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(Shared::default());
        render::queue(
            generation,
            Arc::clone(&shared),
            render::Operation::Open(texture_id),
        );
        RenderingServer::singleton().force_sync();
        if let Some(error) = lock(&shared.error).as_ref() {
            anyhow::bail!("{error}");
        }
        let mut controller = Self {
            input_target: None,
            generation,
            shared,
            material,
            _texture: texture,
            current: None,
            aspect: 16.0 / 9.0,
            worker: None,
            stopped: false,
        };
        let shared = Arc::clone(&controller.shared);
        controller.worker = Some(
            thread::Builder::new()
                .name("weld-video-source".into())
                .spawn(move || {
                    let result = match source {
                        Source::Fixture { single_frame } => play(&shared, single_frame),
                        Source::Iroh { directory, rate } => receiver::run(&shared, directory, rate),
                    };
                    if let Err(error) = result
                        && !shared.cancelled.load(Ordering::Acquire)
                    {
                        shared.fail(format!("video: {error:#}"));
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
        let Some(update) = lock(&self.shared.latest).take() else {
            return Ok(());
        };
        let (frame, display, mut input) = match update {
            PresentationUpdate::Clear => {
                self.current = None;
                self.input_target = None;
                self.uniform("has_frame", false.to_variant())?;
                return Ok(());
            }
            PresentationUpdate::View(view, input) => {
                let Some((geometry, visible)) = self.current else {
                    return Ok(());
                };
                let display = frame::display_geometry(geometry, visible, Some(view))?;
                (None, display, Some(input))
            }
            PresentationUpdate::Frame { frame, view, input } => {
                let display = frame::display_geometry(frame.image.geometry(), frame.visible, view)?;
                self.current = Some((frame.image.geometry(), frame.visible));
                (Some(frame), display, input)
            }
        };
        self.aspect = display.aspect;
        let crop = display.crop;
        self.uniform(
            "crop",
            Vector4::new(crop[0], crop[1], crop[2], crop[3]).to_variant(),
        )?;
        self.uniform("has_frame", true.to_variant())?;
        if let Some(input) = input.as_mut() {
            input.geometry.logical_size = display.logical_size;
        }
        self.input_target = input;
        let Some(frame) = frame else {
            return Ok(());
        };
        *lock(&self.shared.pending) = Some(frame);
        self.shared.queued.store(true, Ordering::Release);
        render::queue(
            self.generation,
            Arc::clone(&self.shared),
            render::Operation::Present,
        );
        Ok(())
    }
    pub fn stop(&mut self) {
        self.reset_input();
        self.input_target = None;
        self.shared.cancelled.store(true, Ordering::Release);
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
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
        let message = lock(&self.shared.message);
        let state = if self.stopped {
            "Stopped"
        } else if let Some(message) = message.as_deref() {
            message
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
    pub fn aspect(&self) -> f32 {
        self.aspect
    }
    pub fn pointer_input(
        &self,
        rectangle: [f64; 4],
        position: InputPosition,
        button: i64,
        pressed: bool,
    ) -> bool {
        if self.stopped || self.shared.cancelled.load(Ordering::Acquire) {
            return false;
        }
        // A release must always get through, even while a new image is binding.
        if self.shared.queued.load(Ordering::Acquire) && (button == 0 || pressed) {
            let handled = lock(&self.shared.input).during_bind(button, pressed);
            self.wake_input();
            return handled;
        }
        let handled = lock(&self.shared.input).pointer(
            self.input_target.as_ref(),
            rectangle,
            position,
            button,
            pressed,
        );
        self.wake_input();
        handled
    }
    pub fn key_input(&self, code: i64, location: i64, pressed: bool, echo: bool) -> bool {
        if self.stopped || self.shared.cancelled.load(Ordering::Acquire) {
            return false;
        }
        let Some(key) = input::physical_key(code, location) else {
            return false;
        };
        let state = if !pressed {
            KeyboardKeyState::Released
        } else if echo {
            KeyboardKeyState::Repeated
        } else {
            KeyboardKeyState::Pressed
        };
        let handled = lock(&self.shared.input).key(key, state);
        self.wake_input();
        handled
    }
    pub fn reset_input(&self) {
        lock(&self.shared.input).reset();
        self.wake_input();
    }
    fn wake_input(&self) {
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }
    pub fn take_cursor(&self) -> Option<ClientCursor> {
        lock(&self.shared.input).take_cursor()
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

fn play(shared: &Shared, single_frame: bool) -> Result<()> {
    let clip = fixture::parse(FIXTURE)?;
    let target = lock(&shared.target)
        .take()
        .context("native import target missing")?;
    let mut decoder = native::Decoder::new(&clip.config, target)?;
    shared.decoder_ready.store(true, Ordering::Release);
    let start = Instant::now();
    for frame in clip
        .frames
        .iter()
        .take(if single_frame { 1 } else { clip.frames.len() })
    {
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
    if single_frame {
        // Qualification: do not submit EOS or a second AU to coax out the first
        // output. A static window must appear without a future client commit.
        let deadline = Instant::now() + Duration::from_secs(3);
        while shared.decoded.load(Ordering::Acquire) == 0
            && !shared.cancelled.load(Ordering::Acquire)
        {
            drain(&mut decoder, shared)?;
            ensure!(
                Instant::now() < deadline,
                "single-AU decode needs future input; unsupported low-delay path"
            );
            thread::sleep(Duration::from_millis(1));
        }
        return Ok(());
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
                shared.publish(Frame::fixture(image), None);
            }
        }
    }
}
