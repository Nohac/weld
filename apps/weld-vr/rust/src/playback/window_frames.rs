//! Two complete window snapshots, not independent layer FIFOs. Retained layers
//! share an owned pixel slot so discarding a snapshot cannot lose their pixels.
use super::observations::{Discard, micros};
use super::{Shared, frame::Frame, input, lock, mailbox::Mailbox, session::Pane};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak, atomic::Ordering},
    time::{Duration, Instant},
};
use weld_client::{InputPosition, SurfaceContentView, SurfaceLayerId};

struct PixelSlot<T> {
    value: Mutex<Option<T>>,
    observer: Weak<Shared>,
    discard: Mutex<Discard>,
}
impl<T> PixelSlot<T> {
    fn new(value: T, observer: &Arc<Shared>) -> Arc<Self> {
        observer.decoded.fetch_add(1, Ordering::Relaxed);
        observer
            .session
            .observations
            .decoded
            .fetch_add(1, Ordering::Relaxed);
        Arc::new(Self {
            value: Mutex::new(Some(value)),
            observer: Arc::downgrade(observer),
            discard: Mutex::new(Discard::Lifecycle),
        })
    }
    fn take(&self) -> Option<T> {
        lock(&self.value).take()
    }
}
impl<T> Drop for PixelSlot<T> {
    fn drop(&mut self) {
        if lock(&self.value).is_some()
            && let Some(observer) = self.observer.upgrade()
        {
            observer.session.observations.discard(*lock(&self.discard));
            if !observer.session.cancelled.load(Ordering::Acquire) {
                observer.replaced.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Clone, Default)]
struct LayerContent {
    image: Option<Arc<PixelSlot<Frame>>>,
    extent: Option<[u32; 2]>,
    view: Option<SurfaceContentView>,
    input: Option<input::Target>,
}

#[derive(PartialEq)]
struct LayerLayout {
    layer: SurfaceLayerId,
    visible: bool,
    position: [f32; 2],
    size: [f32; 2],
    stack: i32,
    extent: Option<[u32; 2]>,
    view: Option<SurfaceContentView>,
    origin: Option<InputPosition>,
}
#[derive(PartialEq)]
struct Layout {
    mapped: bool,
    root: Option<SurfaceLayerId>,
    layers: Vec<LayerLayout>,
}

type Snapshot = Vec<(Arc<Shared>, LayerContent)>;
pub(super) struct WindowFrames {
    content: BTreeMap<SurfaceLayerId, LayerContent>,
    pending: Mailbox<Snapshot>,
    layout: Option<Layout>,
    last_arrival: Option<Instant>,
}
impl Default for WindowFrames {
    fn default() -> Self {
        Self {
            content: BTreeMap::new(),
            pending: Mailbox::smoothing(Duration::from_nanos(1_000_000_000 / 60)),
            layout: None,
            last_arrival: None,
        }
    }
}
impl WindowFrames {
    pub fn update(
        &mut self,
        layer: SurfaceLayerId,
        shared: &Arc<Shared>,
        frame: Option<Frame>,
        view: Option<SurfaceContentView>,
        input: Option<input::Target>,
    ) {
        let content = self.content.entry(layer).or_default();
        if let Some(frame) = frame {
            content.extent = Some(frame.visible);
            content.image = Some(PixelSlot::new(frame, shared));
        }
        content.view = view;
        content.input = input;
    }
    pub fn enqueue(
        &mut self,
        panes: &BTreeMap<SurfaceLayerId, Pane>,
        mapped: bool,
        root: Option<SurfaceLayerId>,
        interval: Duration,
    ) {
        self.content.retain(|layer, content| {
            if panes.contains_key(layer) {
                return true;
            }
            if let Some(image) = &content.image {
                *lock(&image.discard) = Discard::Lifecycle;
            }
            false
        });
        let layout = Layout {
            mapped,
            root,
            layers: panes
                .iter()
                .map(|(layer, pane)| {
                    let content = self.content.get(layer);
                    LayerLayout {
                        layer: *layer,
                        visible: pane.visible,
                        position: pane.position,
                        size: pane.size,
                        stack: pane.stack,
                        extent: content.and_then(|content| content.extent),
                        view: content.and_then(|content| content.view),
                        origin: content.and_then(|content| {
                            content.input.as_ref().map(|input| input.geometry.origin)
                        }),
                    }
                })
                .collect(),
        };
        // Geometry/lifecycle changes are immediate barriers. Smoothing applies
        // only to pixels in an unchanged window layout and cannot revive a layer.
        if self.layout.as_ref() != Some(&layout) {
            for snapshot in self.pending.drain() {
                discard(snapshot, Discard::Layout);
            }
            self.layout = Some(layout);
        }
        self.pending.set_interval(interval);
        let snapshot: Snapshot = panes
            .iter()
            .filter_map(|(layer, pane)| {
                self.content
                    .get(layer)
                    .map(|content| (pane.shared.clone(), content.clone()))
            })
            .collect();
        let now = Instant::now();
        if let Some((shared, _)) = snapshot.first()
            && let Some(previous) = self.last_arrival
        {
            let gap = now.duration_since(previous);
            shared
                .session
                .observations
                .commit_gap_max_us
                .fetch_max(micros(gap), Ordering::Relaxed);
            if gap < interval / 2 {
                shared
                    .session
                    .observations
                    .commit_bursts
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        self.last_arrival = Some(now);
        if let Some(snapshot) = self.pending.push(snapshot, now) {
            discard(snapshot, Discard::Superseded);
        }
    }
    pub fn present(&mut self, now: Instant) {
        let (snapshot, age, expired) = self.pending.pop(now);
        if let Some(snapshot) = expired {
            discard(snapshot, Discard::Stale);
        }
        let Some(snapshot) = snapshot else {
            return;
        };
        if let Some((shared, _)) = snapshot.first() {
            shared.session.observations.selected(age);
        }
        for (shared, content) in snapshot {
            if let Some(frame) = content.image.and_then(|slot| slot.take()) {
                shared.publish_input(frame, content.view, content.input);
            } else if let (Some(view), Some(input)) = (content.view, content.input) {
                shared.set_view(view, input);
            }
        }
    }
}

fn discard(snapshot: Snapshot, reason: Discard) {
    // Retained pixels can survive an evicted snapshot. Count on final Drop only,
    // and count nothing if a surviving snapshot eventually consumes the slot.
    for (_, content) in &snapshot {
        if let Some(image) = &content.image {
            *lock(&image.discard) = reason;
        }
    }
}

impl Drop for WindowFrames {
    fn drop(&mut self) {
        for snapshot in self.pending.drain() {
            discard(snapshot, Discard::Lifecycle);
        }
        for content in self.content.values() {
            if let Some(image) = &content.image {
                *lock(&image.discard) = Discard::Lifecycle;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::PresentationUpdate;
    use super::*;
    use weld_client::{ClientId, ClientSourceId, ClientSurfaceId, SurfaceInputGeometry};

    fn panes() -> BTreeMap<SurfaceLayerId, Pane> {
        let client = ClientId::new(ClientSourceId::new(1), 1);
        [1, 2]
            .into_iter()
            .map(|id| {
                (
                    SurfaceLayerId::new(id),
                    Pane {
                        id,
                        client,
                        window: 1,
                        parent: 0,
                        kind: 0,
                        position: [0.0; 2],
                        size: [800.0, 500.0],
                        stack: id as i32,
                        visible: true,
                        shared: Arc::new(Shared::default()),
                    },
                )
            })
            .collect()
    }
    fn stage(
        frames: &mut WindowFrames,
        panes: &BTreeMap<SurfaceLayerId, Pane>,
        epoch: u64,
        width: f32,
        mapped: bool,
    ) {
        for (layer, pane) in panes {
            let view = SurfaceContentView {
                source_x: 0.0,
                source_y: 0.0,
                source_width: width,
                source_height: 500.0,
                logical_width: width,
                logical_height: 500.0,
            };
            let target = input::Target {
                epoch,
                geometry: SurfaceInputGeometry {
                    surface: ClientSurfaceId::new(pane.client, 1),
                    origin: InputPosition::new(0.0, 0.0),
                    logical_size: [f64::from(width), 500.0],
                    inputs: Vec::new(),
                },
            };
            frames.update(*layer, &pane.shared, None, Some(view), Some(target));
        }
        frames.enqueue(
            panes,
            mapped,
            Some(SurfaceLayerId::new(1)),
            Duration::from_millis(11),
        );
    }
    fn epochs(panes: &BTreeMap<SurfaceLayerId, Pane>) -> Vec<u64> {
        panes
            .values()
            .map(
                |pane| match lock(&pane.shared.latest).take(Instant::now()).0 {
                    Some(PresentationUpdate::View(_, target)) => target.epoch,
                    _ => panic!("expected complete view snapshot"),
                },
            )
            .collect()
    }
    #[test]
    fn window_layers_advance_together_and_layout_changes_bypass_queued_pixels() {
        let panes = panes();
        let mut frames = WindowFrames::default();
        stage(&mut frames, &panes, 1, 800.0, true);
        stage(&mut frames, &panes, 2, 800.0, true);
        frames.present(Instant::now());
        assert_eq!(epochs(&panes), vec![1, 1]);
        frames.present(Instant::now());
        assert_eq!(epochs(&panes), vec![2, 2]);
        stage(&mut frames, &panes, 3, 800.0, true);
        stage(&mut frames, &panes, 4, 400.0, true);
        frames.present(Instant::now());
        assert_eq!(
            epochs(&panes),
            vec![4, 4],
            "resize supersedes older geometry"
        );
        stage(&mut frames, &panes, 5, 400.0, true);
        stage(&mut frames, &panes, 6, 400.0, false);
        frames.present(Instant::now());
        assert_eq!(
            epochs(&panes),
            vec![6, 6],
            "unmap cannot replay mapped history"
        );
    }
    #[test]
    fn a_slow_window_does_not_hold_up_another_windows_snapshot() {
        let first = panes();
        let second = panes();
        let mut slow = WindowFrames::default();
        let mut active = WindowFrames::default();
        stage(&mut slow, &first, 1, 800.0, true);
        stage(&mut active, &second, 2, 800.0, true);
        active.present(Instant::now());
        assert_eq!(epochs(&second), vec![2, 2]);
        assert!(
            first
                .values()
                .all(|pane| lock(&pane.shared.latest).newest_mut().is_none())
        );
    }
    #[test]
    fn retained_pixels_survive_snapshot_eviction_and_are_consumed_once() {
        let shared = Arc::new(Shared::default());
        let pixels = PixelSlot::new(42, &shared);
        let now = Instant::now();
        let mut queue = Mailbox::smoothing(Duration::from_millis(11));
        queue.push(pixels.clone(), now);
        queue.push(pixels.clone(), now);
        queue.push(pixels.clone(), now);
        drop(pixels);
        let first = queue.take(now).0.expect("snapshot");
        let next = queue.take(now).0.expect("snapshot");
        assert_eq!(first.take(), Some(42));
        assert_eq!(next.take(), None);
        assert_eq!(shared.replaced.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn unused_pixels_release_and_count_once_after_the_last_snapshot() {
        let shared = Arc::new(Shared::default());
        let pixels = PixelSlot::new(42, &shared);
        let retained = pixels.clone();
        drop(pixels);
        assert_eq!(shared.replaced.load(Ordering::Relaxed), 0);
        drop(retained);
        assert_eq!(shared.replaced.load(Ordering::Relaxed), 1);
        assert_eq!(shared.decoded.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn evicted_retained_pixels_count_only_if_never_consumed_and_use_final_reason() {
        let shared = Arc::new(Shared::default());
        let pixels = PixelSlot::new(42, &shared);
        let retained = pixels.clone();
        *lock(&pixels.discard) = Discard::Superseded;
        drop(pixels);
        assert_eq!(shared.session.observations.count(Discard::Superseded), 0);
        assert_eq!(retained.take(), Some(42));
        drop(retained);
        assert_eq!(shared.session.observations.count(Discard::Superseded), 0);

        let pixels = PixelSlot::new(43, &shared);
        *lock(&pixels.discard) = Discard::Stale;
        drop(pixels);
        assert_eq!(shared.session.observations.count(Discard::Stale), 1);
        assert_eq!(shared.session.observations.count(Discard::Lifecycle), 0);
        assert_eq!(shared.session.observations.count(Discard::Superseded), 0);
    }
}
