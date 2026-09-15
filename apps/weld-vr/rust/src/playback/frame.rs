//! Native output ownership and one global acquired-image budget, including GPU use.
use crate::native::{Geometry, Image};
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread::Thread,
};
use weld_client::SurfaceContentView;
use weld_media::MediaStreamId;

const MAX_FRAMES: usize = 7;
const MAX_TOTAL_FRAMES: usize = 32;

pub(super) struct FrameBudget {
    outstanding: AtomicUsize,
    receiver: Thread,
    streams: std::sync::Mutex<HashMap<MediaStreamId, usize>>,
}
impl FrameBudget {
    pub fn new(receiver: Thread) -> Arc<Self> {
        Arc::new(Self {
            outstanding: AtomicUsize::new(0),
            receiver,
            streams: std::sync::Mutex::new(HashMap::new()),
        })
    }
    pub fn reserve(self: &Arc<Self>, stream: MediaStreamId) -> Option<FrameCredit> {
        let mut streams = super::lock(&self.streams);
        if streams.get(&stream).copied().unwrap_or(0) >= MAX_FRAMES {
            return None;
        }
        self.outstanding
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_TOTAL_FRAMES).then_some(count + 1)
            })
            .ok()
            .map(|_| {
                *streams.entry(stream).or_default() += 1;
                FrameCredit {
                    budget: self.clone(),
                    notify: true,
                    stream,
                }
            })
    }
}
pub(super) struct FrameCredit {
    budget: Arc<FrameBudget>,
    notify: bool,
    stream: MediaStreamId,
}
impl FrameCredit {
    /// Failed admission cannot wake itself into a Busy retry loop.
    pub fn cancel(mut self) {
        self.notify = false;
    }
}
impl Drop for FrameCredit {
    fn drop(&mut self) {
        self.budget.outstanding.fetch_sub(1, Ordering::AcqRel);
        let mut streams = super::lock(&self.budget.streams);
        if let Some(count) = streams.get_mut(&self.stream) {
            *count -= 1;
            if *count == 0 {
                streams.remove(&self.stream);
            }
        }
        drop(streams);
        if self.notify {
            self.budget.receiver.unpark();
        }
    }
}

/// Field order releases native storage before returning its admission credit.
pub(super) struct Frame {
    pub image: Image,
    pub visible: [u32; 2],
    pub credit: Option<FrameCredit>,
}
impl Frame {
    pub fn fixture(image: Image) -> Self {
        let geometry = image.geometry();
        Self {
            image,
            visible: [
                geometry.crop[2] - geometry.crop[0],
                geometry.crop[3] - geometry.crop[1],
            ],
            credit: None,
        }
    }
    pub fn crop(&self, view: Option<SurfaceContentView>) -> Result<([f32; 4], f32)> {
        crop(self.image.geometry(), self.visible, view)
    }
    pub fn release(self, fence: std::os::fd::OwnedFd) {
        self.image.release(fence);
        drop(self.credit);
    }
}

/// Intersect codec storage crop, transported visible extent, and window content.
pub(super) fn crop(
    geometry: Geometry,
    visible: [u32; 2],
    view: Option<SurfaceContentView>,
) -> Result<([f32; 4], f32)> {
    let display = display_geometry(geometry, visible, view)?;
    Ok((display.crop, display.aspect))
}

pub(super) struct DisplayGeometry {
    pub crop: [f32; 4],
    pub aspect: f32,
    pub logical_size: [f64; 2],
}

pub(super) fn display_geometry(
    geometry: Geometry,
    visible: [u32; 2],
    view: Option<SurfaceContentView>,
) -> Result<DisplayGeometry> {
    let [left, top, right, bottom] = geometry.crop;
    ensure!(
        left < right && top < bottom && right <= geometry.width && bottom <= geometry.height,
        "invalid decoded image crop"
    );
    ensure!(
        visible[0] > 0
            && visible[1] > 0
            && visible[0] <= right - left
            && visible[1] <= bottom - top,
        "decoded image does not cover transported visible extent"
    );
    let view = view.unwrap_or(SurfaceContentView {
        source_x: 0.0,
        source_y: 0.0,
        source_width: visible[0] as f32,
        source_height: visible[1] as f32,
        logical_width: visible[0] as f32,
        logical_height: visible[1] as f32,
    });
    ensure!(
        [
            view.source_x,
            view.source_y,
            view.source_width,
            view.source_height,
            view.logical_width,
            view.logical_height
        ]
        .iter()
        .all(|v| v.is_finite())
            && view.source_x >= 0.0
            && view.source_y >= 0.0
            && view.source_width >= 1.0
            && view.source_height >= 1.0
            && view.logical_width > 0.0
            && view.logical_height > 0.0,
        "invalid transported content view"
    );
    let x_end = (view.source_x + view.source_width).min(visible[0] as f32);
    let y_end = (view.source_y + view.source_height).min(visible[1] as f32);
    ensure!(
        x_end - view.source_x >= 1.0 && y_end - view.source_y >= 1.0,
        "content view is outside decoded image"
    );
    let logical_size = [
        f64::from(view.logical_width * (x_end - view.source_x) / view.source_width),
        f64::from(view.logical_height * (y_end - view.source_y) / view.source_height),
    ];
    let aspect = (logical_size[0] / logical_size[1]) as f32;
    ensure!(
        logical_size
            .iter()
            .all(|value| value.is_finite() && *value > 0.0)
            && aspect.is_finite()
            && aspect > 0.0,
        "invalid clipped logical extent"
    );
    Ok(DisplayGeometry {
        crop: [
            (left as f32 + view.source_x + 0.5) / geometry.width as f32,
            (top as f32 + view.source_y + 0.5) / geometry.height as f32,
            (left as f32 + x_end - 0.5) / geometry.width as f32,
            (top as f32 + y_end - 0.5) / geometry.height as f32,
        ],
        aspect,
        // Sampling uses pixel centers; input covers the complete clipped pixel
        // edges. Both use the same intersection, even during resize mismatch.
        logical_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clipped_logical_underflow_is_rejected() {
        let result = display_geometry(
            Geometry {
                width: 1,
                height: 1,
                crop: [0, 0, 1, 1],
            },
            [1, 1],
            Some(SurfaceContentView {
                source_x: 0.0,
                source_y: 0.0,
                source_width: 1000.0,
                source_height: 1.0,
                logical_width: f32::from_bits(1),
                logical_height: 1.0,
            }),
        );
        assert!(
            result
                .err()
                .expect("underflow must fail")
                .to_string()
                .contains("invalid clipped logical extent")
        );
    }
    #[test]
    fn clipped_video_and_input_use_the_same_source_intersection() {
        let display = display_geometry(
            Geometry {
                width: 100,
                height: 80,
                crop: [0, 0, 100, 80],
            },
            [100, 80],
            Some(SurfaceContentView {
                source_x: 20.0,
                source_y: 10.0,
                source_width: 100.0,
                source_height: 100.0,
                logical_width: 200.0,
                logical_height: 200.0,
            }),
        )
        .expect("display");
        assert_eq!(display.logical_size, [160.0, 140.0]);
        assert!((display.aspect - 160.0 / 140.0).abs() < 1e-6);
        assert_eq!(
            display.crop,
            [20.5 / 100.0, 10.5 / 80.0, 99.5 / 100.0, 79.5 / 80.0]
        );
    }
    #[test]
    fn credits_bound_all_owned_outputs_and_failed_admission_returns_capacity() {
        let budget = FrameBudget::new(std::thread::current());
        let mut credits = (0..7)
            .map(|_| budget.reserve(MediaStreamId::new(1)).expect("credit"))
            .collect::<Vec<_>>();
        assert!(budget.reserve(MediaStreamId::new(1)).is_none());
        credits.pop().expect("reserved").cancel();
        assert!(budget.reserve(MediaStreamId::new(1)).is_some());
        drop(credits);
        assert_eq!(budget.outstanding.load(Ordering::Acquire), 0);
    }
    #[test]
    fn multiple_streams_share_total_budget_without_exhausting_any_reader() {
        let budget = FrameBudget::new(std::thread::current());
        let mut credits = Vec::new();
        for stream in 0..4 {
            for _ in 0..7 {
                credits.push(budget.reserve(MediaStreamId::new(stream)).expect("credit"));
            }
            assert!(budget.reserve(MediaStreamId::new(stream)).is_none());
        }
        for _ in 0..4 {
            credits.push(budget.reserve(MediaStreamId::new(4)).expect("credit"));
        }
        assert!(budget.reserve(MediaStreamId::new(5)).is_none());
        credits.pop().expect("credit").cancel();
        assert!(budget.reserve(MediaStreamId::new(5)).is_some());
        drop(credits);
        assert!(super::super::lock(&budget.streams).is_empty());
    }
    #[test]
    fn odd_visible_height_excludes_av1_padding_and_respects_native_crop_origin() {
        let (uv, _) = crop(
            Geometry {
                width: 1280,
                height: 848,
                crop: [0, 0, 1280, 834],
            },
            [1280, 833],
            None,
        )
        .expect("crop");
        assert!((uv[3] - 832.5 / 848.0).abs() < 0.00001);
        let (uv, _) = crop(
            Geometry {
                width: 100,
                height: 80,
                crop: [4, 8, 96, 72],
            },
            [80, 60],
            None,
        )
        .expect("offset");
        assert_eq!(uv, [4.5 / 100.0, 8.5 / 80.0, 83.5 / 100.0, 67.5 / 80.0]);
        assert!(
            crop(
                Geometry {
                    width: 100,
                    height: 80,
                    crop: [4, 8, 96, 72]
                },
                [100, 80],
                None
            )
            .is_err()
        );
    }
}
