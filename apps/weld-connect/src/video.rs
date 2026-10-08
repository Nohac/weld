//! Dioxus widget importing hardware frames on Blitz's GLES device.
use crate::{media, session::Session};
use anyhow::{Context, Result};
use anyrender::{PaintRef, PaintScene, RenderContext, ResourceId};
use blitz_dom::{Widget, node::ComputedStyles};
use blitz_traits::events::{BlitzPointerId, UiEvent};
use peniko::{
    Fill, ImageBrush, ImageSampler,
    kurbo::{Affine, Rect},
};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use weld_client::{
    Extent, InputPosition, SurfaceContentView, SurfaceInputGeometry, TouchEvent, TouchId,
    WindowPreference,
};
use weld_video_gles as gpu;

pub struct Video {
    session: Arc<Session>,
    converter: Option<gpu::Converter>,
    texture: Option<wgpu::Texture>,
    resource: Option<ResourceId>,
    displayed: Option<(u64, SurfaceContentView, SurfaceInputGeometry)>,
    rectangle: [f64; 4],
    epoch: u64,
    clock: Instant,
    timing: Timing,
}
impl Video {
    pub fn new(session: Arc<Session>) -> Self {
        Self {
            session,
            converter: None,
            texture: None,
            resource: None,
            displayed: None,
            rectangle: [0.0; 4],
            epoch: 0,
            clock: Instant::now(),
            timing: Timing::default(),
        }
    }
    fn clear(&mut self, ctx: &mut dyn RenderContext) {
        if let Some(resource) = self.resource.take() {
            ctx.unregister_resource(resource);
        }
        self.texture = None;
        self.displayed = None;
    }
    fn advance(&mut self, ctx: &mut dyn RenderContext) -> Result<()> {
        // Route-created widgets first encounter the active renderer during paint.
        self.prepare(ctx)?;
        let epoch = self.session.media.epoch.load(Ordering::Acquire);
        if self.epoch != epoch {
            self.clear(ctx);
            self.epoch = epoch;
        }
        let Some(converter) = &mut self.converter else {
            return Ok(());
        };
        let (frame, retired, pending) = {
            let mut frames = self
                .session
                .media
                .frames
                .lock()
                .map_err(|_| anyhow::anyhow!("frame mailbox poisoned"))?;
            let (frame, _, retired) = frames.pop(Instant::now());
            (frame, retired, !frames.is_empty())
        };
        drop(retired);
        if pending {
            self.session.media.redraw();
        }
        if let Some(frame) = frame.filter(|frame| frame.epoch == epoch) {
            let started = Instant::now();
            let texture = converter.convert(frame.image)?;
            self.timing.record(
                started,
                started.saturating_duration_since(frame.ready_at),
                started.elapsed(),
            );
            if self.texture.as_ref() != Some(&texture) {
                if let Some(resource) = self.resource.take() {
                    ctx.unregister_resource(resource);
                }
                self.resource = Some(
                    ctx.try_register_custom_resource(Box::new(texture.clone()))
                        .map_err(|error| anyhow::anyhow!("texture registration: {error:?}"))?,
                );
                self.texture = Some(texture);
            }
            self.displayed = Some((epoch, frame.view, frame.input));
        }
        if self.timing.since.elapsed() >= Duration::from_secs(1) {
            if let Ok(frames) = self.session.media.frames.lock() {
                let stats = frames.stats();
                tracing::debug!(target: "weld_media_diag", elapsed_us = self.timing.since.elapsed().as_micros(),
                    presented = self.timing.presented, queue_age_total_us = self.timing.age.as_micros(),
                    queue_age_max_us = self.timing.max_age.as_micros(), import_total_us = self.timing.import.as_micros(),
                    max_gap_us = self.timing.max_gap.as_micros(), submitted = stats.submitted, superseded = stats.superseded, stale = stats.stale,
                    "native client presentation");
            }
            self.timing = Timing::default();
        }
        Ok(())
    }
    fn pause(&mut self) {
        self.timing = Timing::default();
        self.session.media.active.store(false, Ordering::Release);
        self.session.media.reset.store(true, Ordering::Release);
        self.displayed = None;
        self.session.wake();
    }
    fn prepare(&mut self, ctx: &mut dyn RenderContext) -> Result<()> {
        if self.converter.is_some() {
            return Ok(());
        }
        let handle = ctx
            .renderer_specific_context()
            .context("renderer context")?
            .downcast::<wgpu_context::DeviceHandle>()
            .map_err(|_| anyhow::anyhow!("WGPU renderer required"))?;
        self.converter = Some(gpu::converter(&handle)?);
        self.session.media.active.store(true, Ordering::Release);
        self.session.wake();
        tracing::info!("stream presenter ready");
        Ok(())
    }
}

struct Timing {
    since: Instant,
    last: Option<Instant>,
    presented: u64,
    age: Duration,
    max_age: Duration,
    import: Duration,
    max_gap: Duration,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            since: Instant::now(),
            last: None,
            presented: 0,
            age: Duration::ZERO,
            max_age: Duration::ZERO,
            import: Duration::ZERO,
            max_gap: Duration::ZERO,
        }
    }
}
impl Timing {
    fn record(&mut self, now: Instant, age: Duration, import: Duration) {
        self.presented += 1;
        self.age += age;
        self.max_age = self.max_age.max(age);
        self.import += import;
        if let Some(last) = self.last.replace(now) {
            self.max_gap = self.max_gap.max(now.saturating_duration_since(last));
        }
    }
}
impl Widget for Video {
    fn disconnected(&mut self) {
        self.pause();
    }
    fn can_create_surfaces(&mut self, ctx: &mut dyn RenderContext) {
        if let Err(error) = self.prepare(ctx) {
            tracing::error!(%error, "GPU importer failed");
            if let Ok(mut view) = self.session.snapshot.lock() {
                view.status = format!("GPU importer failed: {error:#}");
            }
        }
    }
    fn destroy_surfaces(&mut self) {
        self.pause();
        self.converter = None;
        self.texture = None;
    }
    fn paint(
        &mut self,
        ctx: &mut dyn RenderContext,
        _: &ComputedStyles,
        width: u32,
        height: u32,
        scale: f64,
    ) -> anyrender::Scene {
        let mut scene = anyrender::Scene::new();
        if width == 0 || height == 0 || !scale.is_finite() || scale <= 0.0 {
            return scene;
        }
        let logical = [f64::from(width) / scale, f64::from(height) / scale];
        let preference = WindowPreference {
            size: Extent::new(
                logical[0].round().max(1.0) as u32,
                logical[1].round().max(1.0) as u32,
            ),
            scale_120: (scale * 120.0).round() as u32,
        };
        if let Ok(mut current) = self.session.media.preference.lock()
            && *current != Some(preference)
        {
            *current = Some(preference);
            self.session.media.reset.store(true, Ordering::Release);
            self.session.wake();
        }
        if let Err(error) = self.advance(ctx) {
            tracing::error!(%error, "video presentation failed");
            self.clear(ctx);
            self.pause();
            return scene;
        }
        if let (Some(resource), Some((_, view, _))) = (self.resource, &self.displayed) {
            let fit = (f64::from(width) / f64::from(view.logical_width))
                .min(f64::from(height) / f64::from(view.logical_height));
            let w = f64::from(view.logical_width) * fit;
            let h = f64::from(view.logical_height) * fit;
            let x = (f64::from(width) - w) / 2.0;
            let y = (f64::from(height) - h) / 2.0;
            self.rectangle = [x / scale, y / scale, w / scale, h / scale];
            let source_x = f64::from(view.source_x);
            let source_y = f64::from(view.source_y);
            let transform = Affine::translate((x, y))
                * Affine::scale_non_uniform(
                    w / f64::from(view.source_width),
                    h / f64::from(view.source_height),
                )
                * Affine::translate((-source_x, -source_y));
            scene.fill(
                Fill::NonZero,
                transform,
                PaintRef::Resource(ImageBrush {
                    image: resource,
                    sampler: ImageSampler::default(),
                }),
                None,
                &Rect::new(
                    source_x,
                    source_y,
                    source_x + f64::from(view.source_width),
                    source_y + f64::from(view.source_height),
                ),
            );
        }
        scene
    }
    fn handle_event(&mut self, event: &UiEvent) {
        let Some((epoch, _, geometry)) = &self.displayed else {
            return;
        };
        if *epoch != self.session.media.epoch.load(Ordering::Acquire) {
            return;
        }
        let pointer = match event {
            UiEvent::PointerDown(pointer)
            | UiEvent::PointerMove(pointer)
            | UiEvent::PointerUp(pointer)
            | UiEvent::PointerCancel(pointer) => pointer,
            _ => return,
        };
        let id = match pointer.id {
            BlitzPointerId::Finger(id) => TouchId(id),
            BlitzPointerId::Mouse => TouchId(u64::MAX),
            BlitzPointerId::Pen => TouchId(u64::MAX - 1),
        };
        let position =
            InputPosition::new(f64::from(pointer.element.x), f64::from(pointer.element.y));
        let route = geometry.pointer_route(self.rectangle, position);
        let touch = match event {
            UiEvent::PointerDown(_) => TouchEvent::Down { id, position },
            UiEvent::PointerMove(_) => TouchEvent::Motion { id, position },
            UiEvent::PointerUp(_) => TouchEvent::Up { id },
            _ => TouchEvent::Cancel,
        };
        let time = self.clock.elapsed().as_millis() as u32;
        self.session.input(media::Input {
            epoch: *epoch,
            route,
            event: touch,
            time,
        });
        self.session.input(media::Input {
            epoch: *epoch,
            route: None,
            event: TouchEvent::Frame,
            time,
        });
    }
}
impl Drop for Video {
    fn drop(&mut self) {
        self.pause();
    }
}
