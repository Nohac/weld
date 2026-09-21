//! Local panel preview, independent of transport and decoded-image ownership.
use godot::prelude::*;
use std::time::{Duration, Instant};

pub(super) const RESIZE_HANDLE_GAP: f32 = 0.02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}
impl Corner {
    pub fn sign(self) -> Vector2 {
        match self {
            Self::TopLeft => Vector2::new(-1.0, -1.0),
            Self::TopRight => Vector2::new(1.0, -1.0),
            Self::BottomLeft => Vector2::new(-1.0, 1.0),
            Self::BottomRight => Vector2::ONE,
        }
    }
}

pub(super) fn centered_resize(
    start: Vector2,
    delta: Vector2,
    corner: Corner,
    stereo: bool,
) -> Vector2 {
    let desired = start + delta * corner.sign() * 2.0;
    if !desired.is_finite() {
        return start;
    }
    if stereo {
        let ratio = (desired / start).dot(Vector2::ONE) * 0.5;
        let maximum = 3.5 / start.x.max(start.y);
        let minimum = (0.2 / start.x.min(start.y)).min(maximum);
        return start * ratio.clamp(minimum, maximum);
    }
    desired.clamp(Vector2::splat(0.2), Vector2::splat(3.5))
}

pub(super) fn fitted(container: Vector2, logical: Vector2) -> Vector2 {
    logical * (container.x / logical.x).min(container.y / logical.y)
}

enum Preview {
    Drag {
        original: Vector2,
        logical: Vector2,
    },
    Waiting {
        logical: Vector2,
        since: Instant,
        scale: Option<u32>,
    },
}
#[derive(Default)]
pub(super) struct Sizing {
    physical: Option<Vector2>,
    preview: Option<Preview>,
}
impl Sizing {
    pub fn physical(&self, natural: Vector2, logical: Vector2) -> Vector2 {
        let Some(size) = self.physical else {
            return natural;
        };
        if self.preview.is_some() {
            size
        } else {
            fitted(size, logical)
        }
    }
    pub fn busy(&self) -> bool {
        self.preview.is_some()
    }
    pub fn begin(&mut self, physical: Vector2, logical: Vector2) -> bool {
        if self.busy()
            || !physical.is_finite()
            || !logical.is_finite()
            || physical.x <= 0.0
            || physical.y <= 0.0
            || logical.x <= 0.0
            || logical.y <= 0.0
        {
            return false;
        }
        self.physical = Some(physical);
        self.preview = Some(Preview::Drag {
            original: physical,
            logical,
        });
        true
    }
    pub fn update(&mut self, size: Vector2) {
        if size.is_finite()
            && size.x > 0.0
            && size.y > 0.0
            && matches!(self.preview, Some(Preview::Drag { .. }))
        {
            self.physical = Some(size);
        }
    }
    pub fn desired(&self) -> Option<Vector2> {
        let Preview::Drag { original, logical } = self.preview.as_ref()? else {
            return None;
        };
        Some(*logical * (self.physical? / *original))
    }
    pub fn cancel(&mut self) {
        if let Some(Preview::Drag { original, .. }) = self.preview {
            self.physical = Some(original);
            self.preview = None;
        }
    }
    pub fn wait(&mut self, logical: Vector2, scale: Option<u32>, now: Instant) {
        if let Some(desired) = self.desired()
            && scale.is_none()
        {
            // A receiver limit may have reduced the request proportionally.
            self.physical = self.physical.map(|size| size * (logical / desired));
        }
        self.preview = Some(Preview::Waiting {
            logical,
            scale,
            since: now,
        });
    }
    /// Only the displayed matching size completes a request. Ignored or
    /// constrained requests time out; never indefinitely block shell controls.
    pub fn observe(&mut self, logical: Vector2, now: Instant) -> Option<u32> {
        let Some(Preview::Waiting {
            logical: expected,
            since,
            scale,
        }) = self.preview
        else {
            return None;
        };
        let difference = (logical - expected).abs();
        if difference.x.max(difference.y) < 1.0 {
            self.preview = None;
            return scale;
        }
        if now.saturating_duration_since(since) >= Duration::from_secs(2) {
            self.preview = None;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn constrained_or_ignored_size_does_not_leave_a_permanent_preview() {
        let mut state = Sizing::default();
        let now = Instant::now();
        state.begin(Vector2::ONE, Vector2::splat(500.0));
        state.wait(Vector2::splat(800.0), Some(240), now);
        assert_eq!(
            state.observe(Vector2::splat(600.0), now + Duration::from_secs(2)),
            None
        );
        assert!(!state.busy());
        assert!(state.begin(Vector2::ONE, Vector2::splat(600.0)));
    }
    #[test]
    fn all_corners_resize_symmetrically_and_stereo_preserves_aspect() {
        let start = Vector2::new(1.6, 1.0);
        for corner in [
            Corner::TopLeft,
            Corner::TopRight,
            Corner::BottomLeft,
            Corner::BottomRight,
        ] {
            let size = centered_resize(start, corner.sign() * 0.1, corner, false);
            assert!((size - Vector2::new(1.8, 1.2)).length() < 1e-5);
            let stereo = centered_resize(start, corner.sign() * 0.1, corner, true);
            assert!((stereo.x / stereo.y - 1.6).abs() < 1e-5);
        }
    }
    #[test]
    fn preview_waits_for_matching_frame_and_cancellation_restores_size() {
        let mut state = Sizing::default();
        let now = Instant::now();
        let logical = Vector2::new(800.0, 500.0);
        assert!(state.begin(Vector2::new(1.6, 1.0), logical));
        state.update(Vector2::new(2.0, 1.5));
        assert_eq!(state.desired(), Some(Vector2::new(1000.0, 750.0)));
        state.cancel();
        assert!(!state.busy());
        assert!((state.physical(Vector2::ONE, logical) - Vector2::new(1.6, 1.0)).length() < 1e-5);
        assert!(state.begin(Vector2::new(1.6, 1.0), logical));
        state.wait(Vector2::new(640.0, 400.0), Some(240), now);
        assert_eq!(state.observe(logical, now), None);
        assert!(state.busy());
        assert_eq!(state.observe(Vector2::new(640.0, 400.0), now), Some(240));
        assert_eq!(state.observe(Vector2::new(640.0, 400.0), now), None);
        assert!(
            (state.physical(Vector2::ONE, Vector2::new(640.0, 400.0)) - Vector2::new(1.6, 1.0))
                .length()
                < 1e-5
        );
    }
}
