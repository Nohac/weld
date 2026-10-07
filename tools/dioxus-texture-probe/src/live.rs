//! CSS presentation of one live composed stream, backed by Weld's receiver.
use crate::{
    platform,
    session::{Session, Settings, Shared},
};
use anyhow::{Context, Result};
use anyrender::{PaintRef, PaintScene, RenderContext, ResourceId};
use blitz_dom::{Widget, node::ComputedStyles};
use dioxus_native::{CustomWidgetAttr, prelude::*};
use peniko::{
    Fill, ImageBrush, ImageSampler,
    kurbo::{Affine, Rect},
};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

pub fn launch() -> Result<()> {
    let settings = Settings::load(crate::DIRECTORY.get().context("probe directory missing")?)?;
    anyrender_vello_hybrid::set_probe_nonblocking_poll(settings.nonblocking_poll);
    dioxus_native::launch(app);
    Ok(())
}
fn app() -> Element {
    let window = dioxus_native::use_window();
    let session = use_hook(move || -> Result<Arc<Session>, String> {
        let directory = crate::DIRECTORY.get().ok_or("probe directory missing")?;
        let settings = Settings::load(directory).map_err(|error| error.to_string())?;
        platform::consume_settings(directory).map_err(|error| error.to_string())?;
        Session::start(
            directory.clone(),
            settings,
            Arc::new(move || window.request_redraw()),
        )
        .map(Arc::new)
        .map_err(|error| error.to_string())
    });
    let session = match session {
        Ok(session) => session,
        Err(error) => return rsx! { p { "{error}" } },
    };
    let widget = use_hook({
        let shared = session.shared.clone();
        move || CustomWidgetAttr::new(Video::new(shared))
    });
    let mut status = use_signal(String::new);
    use_future({
        let shared = session.shared.clone();
        move || {
            let shared = shared.clone();
            async move {
                loop {
                    futures_timer::Delay::new(Duration::from_millis(250)).await;
                    if let Ok(text) = shared.status.lock()
                        && *status.peek() != *text
                    {
                        status.set(text.clone());
                    }
                }
            }
        }
    });
    rsx! {
        style { {include_str!("style.css")} }
        main {
            div { class: "badge", "LIVE NATIVE GPU PROBE" }
            h1 { "Weld + Dioxus" }
            div { class: "video", object { "data": widget } }
            p { class: "status", "{status}" }
            button { onclick: move |_| { session.shared.paused.fetch_xor(true, Ordering::AcqRel); session.wake(); }, "Pause / resume" }
        }
    }
}
struct Video {
    shared: Arc<Shared>,
    converter: Option<platform::Converter>,
    texture: Option<wgpu::Texture>,
    resource: Option<ResourceId>,
    sampled: u64,
    import_micros: u64,
    age_micros: u64,
    decode_age_micros: u64,
    decode_samples: u64,
    maximum_gap: Duration,
    last_frame: Option<Instant>,
    report: Instant,
    timing: crate::timing::PaintTiming,
}
impl Video {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            converter: None,
            texture: None,
            resource: None,
            sampled: 0,
            import_micros: 0,
            age_micros: 0,
            decode_age_micros: 0,
            decode_samples: 0,
            maximum_gap: Duration::ZERO,
            last_frame: None,
            report: Instant::now(),
            timing: crate::timing::PaintTiming::default(),
        }
    }
    fn advance(&mut self, ctx: &mut dyn RenderContext) -> Result<()> {
        if self.shared.clear.swap(false, Ordering::AcqRel) {
            if let Some(id) = self.resource.take() {
                ctx.unregister_resource(id);
            }
            self.texture = None;
        }
        if self.shared.paused.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.converter.is_none() {
            return Ok(());
        }
        let (frame, retired, pending) = {
            let mut frames = self
                .shared
                .frames
                .lock()
                .map_err(|_| anyhow::anyhow!("frame mailbox poisoned"))?;
            let (frame, _, retired) = frames.pop(Instant::now());
            (frame, retired, !frames.is_empty())
        };
        drop(retired);
        if pending {
            self.shared.followup_redraw();
        }
        if let Some(frame) = frame {
            let start = Instant::now();
            self.age_micros = self
                .age_micros
                .saturating_add(micros(start.duration_since(frame.ready)));
            if let Some(decoded_at) = frame.decoded_at {
                self.decode_samples += 1;
                self.decode_age_micros = self
                    .decode_age_micros
                    .saturating_add(micros(start.saturating_duration_since(decoded_at)));
            }
            let texture = self
                .converter
                .as_mut()
                .context("GPU importer missing")?
                .convert(frame.image)?;
            self.import_micros = self.import_micros.saturating_add(micros(start.elapsed()));
            if self.texture.as_ref() != Some(&texture) {
                if let Some(id) = self.resource.take() {
                    ctx.unregister_resource(id);
                }
                self.resource = Some(
                    ctx.try_register_custom_resource(Box::new(texture.clone()))
                        .map_err(|error| anyhow::anyhow!("texture registration: {error:?}"))?,
                );
                self.texture = Some(texture);
            }
            if let Some(last) = self.last_frame.replace(start) {
                self.maximum_gap = self.maximum_gap.max(start.duration_since(last));
            }
            self.sampled += 1;
        }
        if self.report.elapsed() >= Duration::from_secs(1) {
            self.timing.report(self.report.elapsed());
            let stats = self
                .shared
                .frames
                .lock()
                .map_err(|_| anyhow::anyhow!("frame mailbox poisoned"))?
                .stats();
            log::info!(
                "probe_present elapsed_ms={} sampled={} import_us={} ready_age_us={} max_gap_us={} submitted={} superseded={} stale={} decode_age_us={} decode_samples={}",
                self.report.elapsed().as_millis(),
                self.sampled,
                self.import_micros,
                self.age_micros,
                self.maximum_gap.as_micros(),
                stats.submitted,
                stats.superseded,
                stats.stale,
                self.decode_age_micros,
                self.decode_samples
            );
            self.sampled = 0;
            self.import_micros = 0;
            self.age_micros = 0;
            self.decode_age_micros = 0;
            self.decode_samples = 0;
            self.maximum_gap = Duration::ZERO;
            self.report = Instant::now();
        }
        Ok(())
    }
}
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

impl Widget for Video {
    fn can_create_surfaces(&mut self, ctx: &mut dyn RenderContext) {
        if self.shared.stopped.load(Ordering::Acquire) {
            return;
        }
        if self.converter.is_some() {
            return;
        }
        if let Some(id) = self.resource.take() {
            ctx.unregister_resource(id);
        }
        let result = (|| -> Result<_> {
            let handle = ctx
                .renderer_specific_context()
                .context("renderer context")?
                .downcast::<wgpu_context::DeviceHandle>()
                .map_err(|_| anyhow::anyhow!("WGPU renderer required"))?;
            log::info!("probe adapter: {:?}", handle.adapter.get_info());
            platform::converter(&handle)
        })();
        match result {
            Ok(converter) => {
                self.converter = Some(converter);
                self.shared.active.store(true, Ordering::Release);
            }
            Err(error) => self.shared.message(format!("Import failed: {error:#}")),
        }
    }
    fn destroy_surfaces(&mut self) {
        self.timing.suspend();
        if let Ok(mut wake) = self.shared.wake.lock() {
            wake.take();
        }
        self.shared.active.store(false, Ordering::Release);
        if let Ok(mut frames) = self.shared.frames.lock() {
            drop(frames.drain());
        }
        self.converter = None;
        self.texture = None;
        self.last_frame = None;
    }
    fn requires_redraw(&self) -> bool {
        false
    }
    fn paint(
        &mut self,
        ctx: &mut dyn RenderContext,
        _: &ComputedStyles,
        width: u32,
        height: u32,
        _: f64,
    ) -> anyrender::Scene {
        let now = Instant::now();
        let requested = self
            .shared
            .wake
            .lock()
            .ok()
            .and_then(|mut wake| wake.take());
        self.timing.paint(now, requested);
        if let Err(error) = self.advance(ctx) {
            self.shared
                .message(format!("Presentation failed: {error:#}"));
            self.shared.stopped.store(true, Ordering::Release);
            self.shared.active.store(false, Ordering::Release);
        }
        let mut scene = anyrender::Scene::new();
        if let (Some(image), Some(texture)) = (self.resource, self.texture.as_ref()) {
            let scale = (f64::from(width) / f64::from(texture.width()))
                .min(f64::from(height) / f64::from(texture.height()));
            let transform = Affine::translate((
                (f64::from(width) - f64::from(texture.width()) * scale) / 2.0,
                (f64::from(height) - f64::from(texture.height()) * scale) / 2.0,
            )) * Affine::scale(scale);
            scene.fill(
                Fill::NonZero,
                transform,
                PaintRef::Resource(ImageBrush {
                    image,
                    sampler: ImageSampler::default(),
                }),
                None,
                &Rect::new(
                    0.0,
                    0.0,
                    f64::from(texture.width()),
                    f64::from(texture.height()),
                ),
            );
        }
        scene
    }
}
