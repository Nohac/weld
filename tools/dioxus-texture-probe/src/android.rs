use crate::{gpu::Converter, playback::Playback};
use anyhow::{Context, Result};
use anyrender::{PaintRef, PaintScene, RenderContext, ResourceId};
use blitz_dom::{Widget, node::ComputedStyles};
use dioxus_native::{CustomWidgetAttr, prelude::*};
use peniko::{
    Fill, ImageBrush, ImageSampler,
    kurbo::{Affine, Rect},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::TryRecvError,
    },
    time::{Duration, Instant},
};
use wgpu_context::DeviceHandle;

#[derive(Default)]
struct Controls {
    paused: AtomicBool,
    replay: AtomicU64,
    status: Mutex<String>,
}
impl Controls {
    fn report(&self, message: String) {
        log::info!("{message}");
        if let Ok(mut status) = self.status.lock() {
            *status = message;
        }
    }
}

pub fn launch() {
    log::info!("Dioxus Native / Blitz texture probe starting");
    dioxus_native::launch(app);
}

fn app() -> Element {
    let controls = use_hook(|| Arc::new(Controls::default()));
    let widget = use_hook({
        let controls = controls.clone();
        move || CustomWidgetAttr::new(VideoWidget::new(controls))
    });
    let mut status = use_signal(|| "Starting native renderer...".to_string());
    use_future({
        let controls = controls.clone();
        move || {
            let controls = controls.clone();
            async move {
                loop {
                    futures_timer::Delay::new(Duration::from_millis(500)).await;
                    if let Ok(text) = controls.status.lock()
                        && !text.is_empty()
                        && *status.peek() != *text
                    {
                        status.set(text.clone());
                    }
                }
            }
        }
    });
    let pause = controls.clone();
    let replay = controls;
    rsx! {
        style { {include_str!("style.css")} }
        main {
            div { class: "badge", "NATIVE GPU PROBE" }
            h1 { "Weld + Dioxus" }
            p { "AV1 • MediaCodec → EGL → WGPU → Blitz" }
            div { class: "video", object { "data": widget } }
            div { class: "actions",
                button { onclick: move |_| { pause.paused.fetch_xor(true, Ordering::AcqRel); }, "Pause / resume" }
                button { onclick: move |_| { replay.paused.store(false, Ordering::Release); replay.replay.fetch_add(1, Ordering::AcqRel); }, "Replay" }
            }
            p { class: "status", "{status}" }
            p { "Red edge at top, blue at bottom. Try rotation and background/resume." }
        }
    }
}

struct VideoWidget {
    controls: Arc<Controls>,
    converter: Option<Converter>,
    playback: Option<Playback>,
    resource: Option<ResourceId>,
    generation: u64,
    next_frame: Instant,
    frames: u32,
    done: bool,
}
impl VideoWidget {
    fn new(controls: Arc<Controls>) -> Self {
        Self {
            controls,
            converter: None,
            playback: None,
            resource: None,
            generation: 0,
            next_frame: Instant::now(),
            frames: 0,
            done: false,
        }
    }
    fn advance(&mut self, ctx: &mut dyn RenderContext) -> Result<()> {
        let generation = self.controls.replay.load(Ordering::Acquire);
        if generation != self.generation {
            self.playback = None;
            self.generation = generation;
            self.frames = 0;
            self.done = false;
        }
        if self.done || self.controls.paused.load(Ordering::Acquire) {
            return Ok(());
        }
        anyhow::ensure!(self.converter.is_some(), "GPU importer unavailable");
        if self.playback.is_none() {
            self.playback = Some(Playback::start()?);
        }
        if Instant::now() < self.next_frame {
            return Ok(());
        }
        let received = self
            .playback
            .as_ref()
            .context("playback")?
            .frames
            .try_recv();
        match received {
            Ok(frame) => {
                let frame = frame.map_err(anyhow::Error::msg)?;
                let converter = self.converter.as_mut().context("GPU importer")?;
                let texture = converter.convert(frame)?;
                if self.resource.is_none() {
                    self.resource = Some(
                        ctx.try_register_custom_resource(Box::new(texture))
                            .map_err(|error| {
                                anyhow::anyhow!("Blitz texture registration: {error:?}")
                            })?,
                    );
                }
                self.frames += 1;
                self.next_frame = Instant::now() + Duration::from_micros(33_333);
                if self.frames == 1 || self.frames.is_multiple_of(30) {
                    self.controls.report(format!(
                        "Presented {} / 120 decoded GPU frames",
                        self.frames
                    ));
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.done = true;
                self.playback = None;
                self.controls.report(format!(
                    "Finished: {} / 120 frames. Tap Replay to repeat.",
                    self.frames
                ));
            }
        }
        Ok(())
    }
}
impl Widget for VideoWidget {
    fn can_create_surfaces(&mut self, ctx: &mut dyn RenderContext) {
        // Blitz's window-resume and first-paint paths can both announce readiness.
        if self.converter.is_some() {
            return;
        }
        if let Some(resource) = self.resource.take() {
            ctx.unregister_resource(resource);
        }
        let result = (|| -> Result<Converter> {
            let handle = ctx
                .renderer_specific_context()
                .context("renderer context")?
                .downcast::<DeviceHandle>()
                .map_err(|_| anyhow::anyhow!("expected WGPU renderer"))?;
            self.controls
                .report(format!("GPU: {:?}", handle.adapter.get_info()));
            Converter::new(&handle.device)
        })();
        match result {
            Ok(converter) => {
                self.converter = Some(converter);
                self.done = false;
                self.frames = 0;
            }
            Err(error) => {
                self.done = true;
                self.controls
                    .report(format!("Import unavailable: {error:#}"));
            }
        }
    }
    fn destroy_surfaces(&mut self) {
        self.playback = None;
        // Retain the ID until a live render context can unregister it on resume.
        self.converter = None;
        self.controls
            .report("Renderer suspended; native resources released".into());
    }
    fn requires_redraw(&self) -> bool {
        self.converter.is_some()
            && ((!self.done && !self.controls.paused.load(Ordering::Acquire))
                || self.generation != self.controls.replay.load(Ordering::Acquire))
    }
    fn paint(
        &mut self,
        ctx: &mut dyn RenderContext,
        _: &ComputedStyles,
        width: u32,
        height: u32,
        _: f64,
    ) -> anyrender::Scene {
        if let Err(error) = self.advance(ctx) {
            self.done = true;
            self.playback = None;
            self.controls
                .report(format!("Presentation failed: {error:#}"));
        }
        let mut scene = anyrender::Scene::new();
        if let Some(image) = self.resource {
            // The fixed fixture's geometry remains independent of the CSS box.
            scene.fill(
                Fill::NonZero,
                Affine::scale_non_uniform(f64::from(width) / 320.0, f64::from(height) / 180.0),
                PaintRef::Resource(ImageBrush {
                    image,
                    sampler: ImageSampler::default(),
                }),
                None,
                &Rect::new(0.0, 0.0, 320.0, 180.0),
            );
        }
        scene
    }
}
