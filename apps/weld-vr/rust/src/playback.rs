//! One GPU presentation session, fed by a fixture or the shared hoist receiver.
//! No Godot object crosses to codec/network/render threads.
//! The shared render adapter retains native images until GPU release fences
//! signal. Unrecoverable context loss quarantines bounded leases and prohibits
//! replay instead of risking buffer reuse.
mod frame;
mod input;
mod mailbox;
mod observations;
mod receiver;
mod receiver_decode;
mod render;
mod retirement;
pub(crate) mod session;
mod stress;
mod window_frames;

use frame::Frame;
use mailbox::Mailbox;

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
    Stress {
        path: PathBuf,
        seconds: u64,
        smooth: bool,
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
struct SessionState {
    gamepad: Mutex<Option<weld_hoist_core::gamepad::GamepadController>>,
    observations: observations::Observations,
    input: Mutex<input::InputState>,
    presentation_rate: Mutex<Option<PresentationRate>>,
    cancelled: AtomicBool,
    error: Mutex<Option<String>>,
    message: Mutex<Option<String>>,
    wake: Mutex<Option<thread::Thread>>,
}

impl SessionState {
    fn set_presentation_rate(&self, rate: PresentationRate) -> bool {
        {
            let mut current = lock(&self.presentation_rate);
            if *current == Some(rate) {
                return false;
            }
            *current = Some(rate);
        }
        if let Some(worker) = lock(&self.wake).as_ref() {
            worker.unpark();
        }
        true
    }
}

#[derive(Default)]
struct Shared {
    diagnostic: Option<stress::Counters>,
    session: Arc<SessionState>,
    closed: AtomicBool,
    epoch: AtomicU64,
    queued: AtomicBool,
    latest: Mutex<Mailbox<PresentationUpdate>>,
    retired: Mutex<Vec<Retired>>,
    target: Mutex<Option<native::Target>>,
    pending: Mutex<Option<Frame>>,
    decoded: AtomicU64,
    presented: AtomicU64,
    replaced: AtomicU64,
    done: AtomicBool,
    decoder_ready: AtomicBool,
}
impl Shared {
    fn publish(&self, frame: Frame, view: Option<SurfaceContentView>) {
        if self.session.cancelled.load(Ordering::Acquire) || self.closed.load(Ordering::Acquire) {
            return;
        }
        self.decoded.fetch_add(1, Ordering::Relaxed);
        self.session
            .observations
            .decoded
            .fetch_add(1, Ordering::Relaxed);
        self.publish_input(frame, view, None, self.epoch.load(Ordering::Acquire));
    }
    fn publish_input(
        &self,
        frame: Frame,
        view: Option<SurfaceContentView>,
        input: Option<input::Target>,
        expected_epoch: u64,
    ) {
        let mut latest = lock(&self.latest);
        if self.session.cancelled.load(Ordering::Acquire) || self.closed.load(Ordering::Acquire) {
            self.session
                .observations
                .discard(observations::Discard::Lifecycle);
            return;
        }
        if self.epoch.load(Ordering::Acquire) != expected_epoch {
            self.session
                .observations
                .discard(observations::Discard::Lifecycle);
            return;
        }
        if matches!(
            latest.push(
                PresentationUpdate::Frame { frame, view, input },
                Instant::now()
            ),
            Some(PresentationUpdate::Frame { .. })
        ) {
            self.replaced.fetch_add(1, Ordering::Relaxed);
            self.session
                .observations
                .discard(observations::Discard::Handoff);
        }
    }
    fn set_view(&self, view: SurfaceContentView, input: input::Target, expected_epoch: u64) {
        let mut latest = lock(&self.latest);
        if self.epoch.load(Ordering::Acquire) != expected_epoch {
            return;
        }
        match latest.newest_mut() {
            Some(PresentationUpdate::Frame {
                view: current,
                input: current_input,
                ..
            }) => {
                *current = Some(view);
                *current_input = Some(input);
            }
            Some(PresentationUpdate::Clear) => {}
            _ => latest.reset(PresentationUpdate::View(view, input)),
        }
    }
    fn clear(&self) {
        let mut latest = lock(&self.latest);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.record_discarded_updates(&mut latest);
        latest.reset(PresentationUpdate::Clear);
    }
    fn discard_pending_updates(&self) {
        self.record_discarded_updates(&mut lock(&self.latest));
    }
    fn record_discarded_updates(&self, latest: &mut Mailbox<PresentationUpdate>) {
        for update in latest.drain() {
            if matches!(update, PresentationUpdate::Frame { .. }) {
                self.session
                    .observations
                    .discard(observations::Discard::Lifecycle);
            }
        }
    }
    fn message(&self, message: impl AsRef<str>) {
        let mut current = lock(&self.session.message);
        if current.as_deref() != Some(message.as_ref()) {
            *current = Some(message.as_ref().to_owned());
        }
    }
    fn fail(&self, error: impl std::fmt::Display) {
        let mut slot = lock(&self.session.error);
        if slot.is_none() {
            *slot = Some(error.to_string());
        }
        self.session.cancelled.store(true, Ordering::Release);
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
    stereo_material: Option<Gd<Object>>,
    _texture: Gd<Object>,
    current: Option<(native::Geometry, [u32; 2])>,
    aspect: f32,
    stopped: bool,
    presented_epoch: u64,
}
impl Controller {
    pub fn start(texture: Gd<Object>, material: Gd<Object>, source: Source) -> Result<Self> {
        let shared = Shared {
            latest: Mutex::new(if matches!(&source, Source::Stress { smooth: true, .. }) {
                Mailbox::smoothing(Duration::from_nanos(1_000_000_000 / 90))
            } else {
                Mailbox::default()
            }),
            diagnostic: matches!(&source, Source::Stress { .. }).then(stress::Counters::default),
            ..Shared::default()
        };
        let mut controller = Self::open(texture, material, Arc::new(shared))?;
        let shared = Arc::clone(&controller.shared);
        controller.worker = Some(
            thread::Builder::new()
                .name("weld-video-source".into())
                .spawn(move || {
                    *lock(&shared.session.wake) = Some(thread::current());
                    let result = match source {
                        Source::Fixture { single_frame } => play(&shared, single_frame),
                        Source::Stress { path, seconds, .. } => {
                            stress::run(&shared, &path, seconds)
                        }
                    };
                    if let Err(error) = result
                        && !shared.session.cancelled.load(Ordering::Acquire)
                    {
                        shared.fail(format!("video: {error:#}"));
                    }
                    shared.done.store(true, Ordering::Release);
                })?,
        );
        Ok(controller)
    }
    fn open(mut texture: Gd<Object>, material: Gd<Object>, shared: Arc<Shared>) -> Result<Self> {
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
        render::queue(
            generation,
            Arc::clone(&shared),
            render::Operation::Open(texture_id),
        );
        RenderingServer::singleton().force_sync();
        if let Some(error) = lock(&shared.session.error).as_ref() {
            anyhow::bail!("{error}");
        }
        let controller = Self {
            input_target: None,
            generation,
            shared,
            material,
            stereo_material: None,
            _texture: texture,
            current: None,
            aspect: 16.0 / 9.0,
            worker: None,
            stopped: false,
            presented_epoch: 0,
        };
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
        if let Some(error) = lock(&self.shared.session.error).as_ref() {
            anyhow::bail!("{error}");
        }
        if let Some(stats) = &self.shared.diagnostic {
            stats.ticks.fetch_add(1, Ordering::Relaxed);
        }
        if self.shared.queued.load(Ordering::Acquire) {
            if let Some(stats) = &self.shared.diagnostic {
                stats.queued.fetch_add(1, Ordering::Relaxed);
            }
            return Ok(());
        }
        if lock(&self.shared.retired).len() >= 2 {
            if let Some(stats) = &self.shared.diagnostic {
                stats.fences.fetch_add(1, Ordering::Relaxed);
            }
            return Ok(());
        }
        let (update, age, dropped, epoch) = {
            let mut latest = lock(&self.shared.latest);
            let (update, age, dropped) = latest.take(Instant::now());
            (
                update,
                age,
                dropped,
                self.shared.epoch.load(Ordering::Acquire),
            )
        };
        self.shared
            .replaced
            .fetch_add(dropped as u64, Ordering::Relaxed);
        let Some(update) = update else {
            if let Some(stats) = &self.shared.diagnostic {
                stats.empty.fetch_add(1, Ordering::Relaxed);
            }
            return Ok(());
        };
        if let Some(stats) = &self.shared.diagnostic {
            let micros = u64::try_from(age.as_micros()).unwrap_or(u64::MAX);
            stats.selection_age_us.fetch_add(micros, Ordering::Relaxed);
            stats
                .selection_age_max_us
                .fetch_max(micros, Ordering::Relaxed);
        }
        self.presented_epoch = epoch;
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
            // Both displayed eyes address one logical interaction view: the
            // left half of the packed client surface. Do not duplicate input.
            if self.stereo_material.is_some() {
                input.geometry.logical_size[0] *= 0.5;
            }
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
        if self.worker.is_some() {
            self.reset_input();
        } else if let Some(target) = &self.input_target {
            lock(&self.shared.session.input).remove_surface(target.geometry.surface);
        }
        self.input_target = None;
        self.shared.closed.store(true, Ordering::Release);
        if self.worker.is_some() {
            self.shared.session.cancelled.store(true, Ordering::Release);
        }
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
        self.shared.discard_pending_updates();
        if lock(&self.shared.pending).take().is_some() {
            self.shared
                .session
                .observations
                .discard(observations::Discard::Lifecycle);
        }
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
        for material in std::iter::once(&mut self.material).chain(self.stereo_material.iter_mut()) {
            material
                .try_call("set_shader_parameter", &[name.to_variant(), value.clone()])
                .map_err(|error| anyhow::anyhow!("material parameter {name}: {error}"))?;
        }
        Ok(())
    }
    /// A second eye samples the same imported texture and frame generation.
    /// Retain its sampler until stop detaches both before native retirement.
    pub fn attach_stereo_material(&mut self, mut material: Gd<Object>) -> Result<()> {
        ensure!(
            self.stereo_material.is_none(),
            "stereo material already attached"
        );
        ensure!(
            material.is_class("ShaderMaterial"),
            "expected a ShaderMaterial"
        );
        for name in ["video", "crop", "has_frame"] {
            let value = self
                .material
                .try_call("get_shader_parameter", &[name.to_variant()])
                .map_err(|error| anyhow::anyhow!("read material parameter {name}: {error}"))?;
            material
                .try_call("set_shader_parameter", &[name.to_variant(), value])
                .map_err(|error| anyhow::anyhow!("stereo material parameter {name}: {error}"))?;
        }
        self.stereo_material = Some(material);
        if let Some(input) = &mut self.input_target {
            input.geometry.logical_size[0] *= 0.5;
        }
        Ok(())
    }
    pub fn finished(&mut self) -> bool {
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(worker) = self.worker.take()
                && worker.join().is_err()
            {
                self.shared.fail("decoder worker panicked");
            }
            self.shared.discard_pending_updates();
        }
        self.worker.is_none()
    }
    pub fn status(&self) -> String {
        if let Some(error) = lock(&self.shared.session.error).as_ref() {
            return error.clone();
        }
        let message = lock(&self.shared.session.message);
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
        let pixels = self.current.map_or([0; 2], |(_, visible)| visible);
        let logical = self
            .input_target
            .as_ref()
            .map_or([0.0; 2], |target| target.geometry.logical_size);
        format!(
            "{state}: decoded {}, presented {}, superseded {}, frame {}x{}, logical {:.0}x{:.0}",
            self.shared.decoded.load(Ordering::Relaxed),
            self.shared.presented.load(Ordering::Relaxed),
            self.shared.replaced.load(Ordering::Relaxed),
            pixels[0],
            pixels[1],
            logical[0],
            logical[1]
        )
    }
    pub fn aspect(&self) -> f32 {
        self.aspect
    }
    pub fn diagnostic_stats(&self) -> Vec<(&'static str, u64)> {
        let Some(stats) = &self.shared.diagnostic else {
            return Vec::new();
        };
        [
            ("selection_age_us", &stats.selection_age_us),
            ("selection_age_max_us", &stats.selection_age_max_us),
            ("submitted", &stats.submitted),
            ("late_submissions", &stats.late),
            ("decoded", &self.shared.decoded),
            ("imported", &self.shared.presented),
            ("superseded", &self.shared.replaced),
            ("ticks", &stats.ticks),
            ("queued_ticks", &stats.queued),
            ("fence_ticks", &stats.fences),
            ("empty_ticks", &stats.empty),
            ("render_wait_us", &stats.render_wait_us),
            ("render_wait_max_us", &stats.render_wait_max_us),
            ("import_us", &stats.import_us),
            ("import_max_us", &stats.import_max_us),
        ]
        .into_iter()
        .map(|(name, value)| (name, value.load(Ordering::Relaxed)))
        .collect()
    }
    pub fn has_frame(&self) -> bool {
        !self.stopped && self.current.is_some()
    }
    pub fn pointer_input(
        &self,
        rectangle: [f64; 4],
        position: InputPosition,
        button: i64,
        pressed: bool,
    ) -> bool {
        if self.stopped || self.shared.session.cancelled.load(Ordering::Acquire) {
            return false;
        }
        // A release must always get through, even while a new image is binding.
        if self.shared.queued.load(Ordering::Acquire) && (button == 0 || pressed) {
            let handled = lock(&self.shared.session.input).during_bind(button, pressed);
            self.wake_input();
            return handled;
        }
        let valid = self.input_token().is_some();
        let handled = lock(&self.shared.session.input).pointer(
            self.input_target.as_ref().filter(|_| valid),
            rectangle,
            position,
            button,
            pressed,
        );
        self.wake_input();
        handled
    }
    /// Identity of currently mapped input, not a native frame/buffer lease.
    pub fn input_token(&self) -> Option<(u64, u64)> {
        let target = self.input_target.as_ref()?;
        let input = lock(&self.shared.session.input);
        (!self.stopped
            && !self.shared.session.cancelled.load(Ordering::Acquire)
            && target.epoch == input.epoch
            && input.surface_visible(target.geometry.surface)
            && self.presented_epoch == self.shared.epoch.load(Ordering::Acquire))
        .then_some((self.generation, target.epoch))
    }
    pub fn input_hit(&self, rectangle: [f64; 4], position: InputPosition) -> bool {
        self.input_token().is_some()
            && self
                .input_target
                .as_ref()
                .is_some_and(|target| target.geometry.pointer_route(rectangle, position).is_some())
    }
    pub fn key_input(&self, code: i64, location: i64, pressed: bool, echo: bool) -> bool {
        if self.stopped || self.shared.session.cancelled.load(Ordering::Acquire) {
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
        let handled = lock(&self.shared.session.input).key(key, state);
        self.wake_input();
        handled
    }
    pub fn reset_input(&self) {
        lock(&self.shared.session.input).reset();
        self.wake_input();
    }
    pub(crate) fn gamepad(&self) -> Option<weld_hoist_core::gamepad::GamepadController> {
        lock(&self.shared.session.gamepad).clone()
    }
    pub fn scroll_input(&self, amount: f64) {
        if self.stopped || self.shared.session.cancelled.load(Ordering::Acquire) {
            return;
        }
        lock(&self.shared.session.input).scroll(amount);
        self.wake_input();
    }
    pub fn close_window(&self) -> bool {
        let accepted = self.input_token().is_some()
            && self
                .input_target
                .as_ref()
                .is_some_and(|target| lock(&self.shared.session.input).close(target));
        self.wake_input();
        accepted
    }
    fn wake_input(&self) {
        if let Some(worker) = lock(&self.shared.session.wake).as_ref() {
            worker.unpark();
        }
    }
    pub fn take_cursor(&self) -> Option<ClientCursor> {
        lock(&self.shared.session.input).take_cursor()
    }
    pub fn logical_size(&self) -> [f64; 2] {
        self.input_target
            .as_ref()
            .map_or([1.0; 2], |target| target.geometry.logical_size)
    }
    pub fn captured(&self) -> bool {
        self.input_target
            .as_ref()
            .is_some_and(|target| lock(&self.shared.session.input).captures(&target.geometry))
    }
    pub fn is_focused(&self) -> bool {
        self.input_token().is_some()
            && self
                .input_target
                .as_ref()
                .is_some_and(|target| lock(&self.shared.session.input).is_focused(target))
    }
    pub fn ready(&self) -> bool {
        if let Err(error) = retirement::reap(&self.shared) {
            self.shared.fail(error);
            return false;
        }
        if self.shared.queued.load(Ordering::Acquire) {
            self.shared
                .session
                .observations
                .render_busy
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if lock(&self.shared.retired).len() >= 2 {
            self.shared
                .session
                .observations
                .fence_busy
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
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
        while !shared.session.cancelled.load(Ordering::Acquire) {
            if start.elapsed().as_micros() >= u128::from(frame.timestamp)
                && decoder.try_send(frame.bytes, frame.timestamp)?
            {
                break;
            }
            drain(&mut decoder, shared)?;
            ensure!(Instant::now() < deadline, "fixture submission timed out");
            thread::sleep(Duration::from_millis(1));
        }
        if shared.session.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        drain(&mut decoder, shared)?;
    }
    if single_frame {
        // Qualification: do not submit EOS or a second AU to coax out the first
        // output. A static window must appear without a future client commit.
        let deadline = Instant::now() + Duration::from_secs(3);
        while shared.decoded.load(Ordering::Acquire) == 0
            && !shared.session.cancelled.load(Ordering::Acquire)
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
    while !shared.session.cancelled.load(Ordering::Acquire) && !decoder.try_finish()? {
        drain(&mut decoder, shared)?;
        ensure!(
            Instant::now() < deadline,
            "fixture EOS submission timed out"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while !shared.session.cancelled.load(Ordering::Acquire) {
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
            .receive(|| shared.session.cancelled.load(Ordering::Acquire))
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
