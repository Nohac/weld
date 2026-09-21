//! Stable XR sizing preferences, not codec capability discovery. The receive
//! ceiling is shared with admission; native frame/decoder counts remain bounded.
use godot::builtin::Projection;
use std::time::{Duration, Instant};
use weld_client::{ClientCommitRevision, ClientSurfaceRequestKind, Extent, SurfaceContentView};

const MAX_DIMENSION: u32 = 2048;
const MAX_PIXELS: u64 = 1920 * 1080;

pub fn supported_extent(width: u32, height: u32) -> bool {
    (1..=MAX_DIMENSION).contains(&width)
        && (1..=MAX_DIMENSION).contains(&height)
        && u64::from(width) * u64::from(height) <= MAX_PIXELS
}

fn bounded_pixels(pixels: [f64; 2]) -> bool {
    pixels
        .iter()
        .all(|v| v.is_finite() && (1.0..=f64::from(MAX_DIMENSION)).contains(v))
        && supported_extent(pixels[0] as u32, pixels[1] as u32)
}

pub(crate) fn configure_fits(
    root: SurfaceContentView,
    content: [f64; 2],
    requested: Extent,
    minimum_scale: f64,
) -> bool {
    let logical = [
        f64::from(root.logical_width),
        f64::from(root.logical_height),
    ];
    let source = [f64::from(root.source_width), f64::from(root.source_height)];
    if logical
        .iter()
        .chain(source.iter())
        .any(|v| !v.is_finite() || *v <= 0.0)
    {
        return false;
    }
    let scale = (source[0] / logical[0])
        .max(source[1] / logical[1])
        .max(minimum_scale);
    let next_root = [
        f64::from(requested.width) + (logical[0] - content[0]).max(0.0),
        f64::from(requested.height) + (logical[1] - content[1]).max(0.0),
    ];
    // Either configure or scale can take effect first. Include decorations at
    // both the observed scale and the desired legacy-integer scale ceiling.
    scale.is_finite() && scale > 0.0 && bounded_pixels(next_root.map(|v| (v * scale).ceil()))
}

/// Bound a user resize at both current and requested scale, including root
/// decorations. Keep the requested aspect rather than clipping one dimension.
pub(crate) fn bounded_resize(
    root: SurfaceContentView,
    content: [f64; 2],
    desired: [f64; 2],
    scale: f64,
) -> Option<Extent> {
    if desired.iter().any(|v| !v.is_finite() || *v < 1.0) || !scale.is_finite() || scale < 1.0 {
        return None;
    }
    let reduction = (f64::from(MAX_DIMENSION) / desired[0].max(desired[1])).min(1.0);
    let desired = desired.map(|v| (v * reduction).floor().max(1.0) as u32);
    let (mut low, mut high, mut best) = (1, desired[0], None);
    while low <= high {
        let width = low + (high - low) / 2;
        let height =
            (u64::from(width) * u64::from(desired[1]) / u64::from(desired[0])).max(1) as u32;
        let candidate = Extent::new(width, height);
        if configure_fits(root, content, candidate, scale) {
            best = Some(candidate);
            low = width + 1;
        } else {
            high = width.saturating_sub(1);
        }
    }
    best
}

pub fn fit(envelope: [f64; 2], aspect: f64) -> Option<[f64; 2]> {
    if !aspect.is_finite()
        || aspect <= 0.0
        || envelope
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0)
    {
        return None;
    }
    let width = envelope[0].min(envelope[1] * aspect);
    let height = width / aspect;
    (width > 0.0 && height > 0.0).then_some([width, height])
}

#[derive(Clone, Copy, Debug)]
pub struct XrPreferences {
    envelope: [f64; 2],
    pixels_per_meter: f64,
    scale_120: u32,
}

impl XrPreferences {
    pub(crate) fn scale_120(&self) -> u32 {
        self.scale_120
    }
    pub fn new(
        eye: [f64; 2],
        projections: &[Projection],
        envelope: [f64; 2],
        distance: f64,
        scale: f64,
        sampling: f64,
    ) -> Option<Self> {
        if !(1..=2).contains(&projections.len())
            || eye
                .iter()
                .any(|v| !v.is_finite() || !(1.0..=16384.0).contains(v))
            || envelope
                .iter()
                .any(|v| !v.is_finite() || !(0.1..=10.0).contains(v))
            || !distance.is_finite()
            || !(0.2..=10.0).contains(&distance)
            || !scale.is_finite()
            || !(1.0..=3.0).contains(&scale)
            || !sampling.is_finite()
            || !(0.5..=3.0).contains(&sampling)
        {
            return None;
        }
        let mut density: f64 = 0.0;
        for projection in projections {
            let [x, y, z, w] = projection.cols;
            if !projection.cols.iter().all(|column| column.is_finite())
                || x.x <= 0.0
                || y.y <= 0.0
                || (z.w + 1.0).abs() > 0.001
                || w.w.abs() > 0.001
                || w.z >= 0.0
            {
                return None;
            }
            // Off-axis terms shift the projected center, not this derivative
            // for a nominal front-facing panel at fixed depth.
            density = density
                .max(eye[0] * f64::from(x.x) / 2.0 / distance)
                .max(eye[1] * f64::from(y.y) / 2.0 / distance);
        }
        // Finite scale has been bounded to [1, 3] before integer conversion.
        Some(Self {
            envelope,
            pixels_per_meter: density * sampling,
            scale_120: (scale * 120.0).round() as u32,
        })
    }

    pub fn panel_size(&self, aspect: f64) -> Option<[f64; 2]> {
        fit(self.envelope, aspect)
    }

    pub fn pixels(&self, aspect: f64) -> Option<[u32; 2]> {
        let size = self.panel_size(aspect)?;
        let width = size[0] * self.pixels_per_meter;
        let height = size[1] * self.pixels_per_meter;
        let reduction = (f64::from(MAX_DIMENSION) / width.max(height))
            .min((MAX_PIXELS as f64 / (width * height)).sqrt())
            .min(1.0);
        // Positive finite sizes are clamped to the receive ceiling before casts.
        let result = [
            (width * reduction).floor() as u32,
            (height * reduction).floor() as u32,
        ];
        supported_extent(result[0], result[1]).then_some(result)
    }

    fn logical_size(&self, aspect: f64) -> Option<Extent> {
        let pixels = self.pixels(aspect)?;
        // Legacy wl_surface clients receive ceil(scale), not fractional scale.
        let integer_scale = self.scale_120.div_ceil(120);
        let size = Extent::new(pixels[0] / integer_scale, pixels[1] / integer_scale);
        (size.width > 0 && size.height > 0).then_some(size)
    }

    fn scale_fits(&self, logical: [f64; 2]) -> bool {
        let scale = f64::from(self.scale_120.div_ceil(120));
        let pixels = logical.map(|value| (value * scale).ceil());
        bounded_pixels(pixels)
    }

    fn bounded_logical_size(&self, logical: [f64; 2], root: SurfaceContentView) -> Option<Extent> {
        let desired = self.logical_size(logical[0] / logical[1])?;
        let minimum_scale = f64::from(self.scale_120.div_ceil(120));
        let (mut low, mut high) = (1, desired.width);
        let mut best = None;
        // At most 2048 possible widths. Integer search accounts for per-axis
        // rounding at the area boundary, including large decoration margins.
        for _ in 0..12 {
            if low > high {
                break;
            }
            let width = low + (high - low) / 2;
            // width <= desired.width, so height stays in [1, desired.height].
            let height = (u64::from(width) * u64::from(desired.height) / u64::from(desired.width))
                .max(1) as u32;
            let candidate = Extent::new(width, height);
            if configure_fits(root, logical, candidate, minimum_scale) {
                best = Some(candidate);
                low = width + 1;
            } else {
                high = width.saturating_sub(1);
            }
        }
        best
    }
}

#[derive(Default)]
pub enum ConfigureSizing {
    #[default]
    Initial,
    AwaitingSize(ClientCommitRevision, Extent),
    Complete,
}

impl ConfigureSizing {
    /// Explicit pixel-oriented test preference at scale one. Unlike desktop
    /// text preferences, packed video must keep its declared source aspect.
    pub fn observe_fixed(
        &mut self,
        size: Extent,
        revision: ClientCommitRevision,
        logical: [f64; 2],
        root: SurfaceContentView,
    ) -> Option<ClientSurfaceRequestKind> {
        match *self {
            Self::Initial if configure_fits(root, logical, size, 1.0) => {
                *self = Self::AwaitingSize(revision, size);
                Some(ClientSurfaceRequestKind::Configure {
                    logical_size: size,
                    resizing: false,
                })
            }
            Self::AwaitingSize(after, requested)
                if revision > after
                    && (logical[0] - f64::from(requested.width)).abs() < 1.0
                    && (logical[1] - f64::from(requested.height)).abs() < 1.0
                    && configure_fits(root, logical, requested, 1.0) =>
            {
                *self = Self::Complete;
                Some(ClientSurfaceRequestKind::SetPreferredScale {
                    scale_120: Some(120),
                })
            }
            _ => None,
        }
    }
    /// Observe only the selected mapped root. A later safe commit gates scale:
    /// merely enqueueing Configure does not mean the application accepted it.
    pub fn observe(
        &mut self,
        preferences: XrPreferences,
        revision: ClientCommitRevision,
        logical: [f64; 2],
        root: SurfaceContentView,
    ) -> Option<ClientSurfaceRequestKind> {
        if logical.iter().any(|v| !v.is_finite() || *v <= 0.0) {
            return None;
        }
        match *self {
            Self::Initial => {
                let logical_size = preferences.bounded_logical_size(logical, root)?;
                *self = Self::AwaitingSize(revision, logical_size);
                Some(ClientSurfaceRequestKind::Configure {
                    logical_size,
                    resizing: false,
                })
            }
            Self::AwaitingSize(after, requested)
                if revision > after
                    && preferences.scale_fits([
                        f64::from(root.logical_width),
                        f64::from(root.logical_height),
                    ])
                    && configure_fits(
                        root,
                        logical,
                        requested,
                        f64::from(preferences.scale_120.div_ceil(120)),
                    ) =>
            {
                *self = Self::Complete;
                Some(ClientSurfaceRequestKind::SetPreferredScale {
                    scale_120: Some(preferences.scale_120),
                })
            }
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct RasterSizing {
    current: Option<[u32; 2]>,
    candidate: Option<([u32; 2], Instant)>,
}

impl RasterSizing {
    pub fn update(&mut self, desired: [u32; 2], now: Instant) -> [u32; 2] {
        // Round down so quantization cannot exceed the decoder-area budget.
        let desired = desired.map(|dimension| (dimension / 64 * 64).max(1));
        let Some(current) = self.current else {
            self.current = Some(desired);
            return desired;
        };
        if desired == current {
            self.candidate = None;
        } else if let Some((candidate, since)) = self.candidate {
            if candidate != desired {
                self.candidate = Some((desired, now));
            } else if now.saturating_duration_since(since) >= Duration::from_millis(300) {
                self.current = Some(desired);
                self.candidate = None;
            }
        } else {
            self.candidate = Some((desired, now));
        }
        self.current.unwrap_or(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interactive_resize_preserves_aspect_and_reserves_hidpi_and_decoration_space() {
        let view = root([820.0, 520.0], 2.0);
        let size = bounded_resize(view, [800.0, 500.0], [2400.0, 1500.0], 3.0).unwrap();
        assert!(configure_fits(view, [800.0, 500.0], size, 3.0));
        assert!((f64::from(size.width) / f64::from(size.height) - 1.6).abs() < 0.01);
        assert!(size.width < 800);
        assert!(bounded_resize(view, [800.0, 500.0], [f64::NAN, 50.0], 2.0).is_none());
    }
    fn root(logical: [f32; 2], scale: f32) -> SurfaceContentView {
        SurfaceContentView {
            source_x: 0.0,
            source_y: 0.0,
            source_width: logical[0] * scale,
            source_height: logical[1] * scale,
            logical_width: logical[0],
            logical_height: logical[1],
        }
    }
    fn preferences() -> XrPreferences {
        XrPreferences::new(
            [2160.0; 2],
            &[Projection::create_perspective(
                90.0, 1.0, 0.05, 100.0, false,
            )],
            [1.6, 1.0],
            1.6,
            1.8,
            2.0,
        )
        .expect("XR preferences")
    }

    #[test]
    fn fixed_packed_size_waits_for_observed_extent_before_setting_scale() {
        let mut sizing = ConfigureSizing::default();
        let size = Extent::new(1600, 480);
        assert!(
            matches!(sizing.observe_fixed(size, ClientCommitRevision::new(1), [800.0, 500.0], root([800.0, 500.0], 1.0)), Some(ClientSurfaceRequestKind::Configure { logical_size, .. }) if logical_size == size)
        );
        assert!(
            sizing
                .observe_fixed(
                    size,
                    ClientCommitRevision::new(2),
                    [800.0, 500.0],
                    root([800.0, 500.0], 1.0)
                )
                .is_none()
        );
        assert!(matches!(
            sizing.observe_fixed(
                size,
                ClientCommitRevision::new(3),
                [1600.0, 480.0],
                root([1600.0, 480.0], 1.0)
            ),
            Some(ClientSurfaceRequestKind::SetPreferredScale {
                scale_120: Some(120)
            })
        ));
    }
    #[test]
    fn portrait_and_landscape_fit_without_exceeding_receive_budget() {
        let preferences = preferences();
        for aspect in [0.5, 1.0, 1.5, 16.0 / 9.0, 4.0] {
            let panel = preferences.panel_size(aspect).expect("panel");
            assert!((panel[0] / panel[1] - aspect).abs() < 1e-8);
            let pixels = preferences.pixels(aspect).expect("pixels");
            assert!(supported_extent(pixels[0], pixels[1]));
            let raster = RasterSizing::default().update(pixels, Instant::now());
            assert!(supported_extent(raster[0], raster[1]));
            assert!(raster[0] <= pixels[0] && raster[1] <= pixels[1]);
            let logical = preferences.logical_size(aspect).expect("logical");
            assert!(preferences.scale_fits([f64::from(logical.width), f64::from(logical.height)]));
        }
        assert!(preferences.pixels(1.5).expect("pixels")[0] > 960);
        assert!(fit([1.0; 2], f64::NAN).is_none());
        assert!(
            XrPreferences::new(
                [2160.0; 2],
                &[Projection::IDENTITY],
                [1.6, 1.0],
                1.6,
                1.8,
                2.0
            )
            .is_none()
        );
    }
    #[test]
    fn scale_waits_for_a_new_safe_commit_and_never_repeats() {
        let preferences = preferences();
        let mut sizing = ConfigureSizing::default();
        let revision = ClientCommitRevision::new(2);
        assert!(matches!(
            sizing.observe(
                preferences,
                revision,
                [1600.0, 1000.0],
                root([1600.0, 1000.0], 1.0)
            ),
            Some(ClientSurfaceRequestKind::Configure { .. })
        ));
        assert!(
            sizing
                .observe(
                    preferences,
                    revision,
                    [800.0, 500.0],
                    root([800.0, 500.0], 1.0)
                )
                .is_none()
        );
        assert!(
            sizing
                .observe(
                    preferences,
                    ClientCommitRevision::new(3),
                    [800.0, 500.0],
                    root([1600.0, 1000.0], 1.0)
                )
                .is_none()
        );
        assert_eq!(
            sizing.observe(
                preferences,
                ClientCommitRevision::new(4),
                [800.0, 500.0],
                root([800.0, 500.0], 1.0)
            ),
            Some(ClientSurfaceRequestKind::SetPreferredScale {
                scale_120: Some(216)
            })
        );
        assert!(
            sizing
                .observe(
                    preferences,
                    ClientCommitRevision::new(5),
                    [800.0, 500.0],
                    root([800.0, 500.0], 1.0)
                )
                .is_none()
        );
    }
    #[test]
    fn initial_resize_accounts_for_existing_hidpi_scale() {
        let mut sizing = ConfigureSizing::default();
        let root = root([800.0, 500.0], 2.25);
        assert!(supported_extent(1800, 1125));
        let request = sizing
            .observe(
                preferences(),
                ClientCommitRevision::new(1),
                [800.0, 500.0],
                root,
            )
            .expect("smaller safe configure");
        let ClientSurfaceRequestKind::Configure { logical_size, .. } = request else {
            panic!("configure expected")
        };
        assert!(logical_size.width < 910);
        assert!(configure_fits(root, [800.0, 500.0], logical_size, 2.0));
        let mut invalid = root;
        invalid.logical_width = 0.0;
        assert!(
            sizing
                .observe(
                    preferences(),
                    ClientCommitRevision::new(2),
                    [800.0, 500.0],
                    invalid
                )
                .is_none()
        );
    }

    #[test]
    fn scale_checks_pending_target_after_decoration_changes() {
        let mut sizing = ConfigureSizing::default();
        assert!(matches!(
            sizing.observe(
                preferences(),
                ClientCommitRevision::new(1),
                [400.0, 200.0],
                root([400.0, 200.0], 1.0)
            ),
            Some(ClientSurfaceRequestKind::Configure { .. })
        ));
        // The observed root fits scale 2, but the pending larger content plus
        // these new margins would not. A pre-configure repaint is not an ACK.
        assert!(
            sizing
                .observe(
                    preferences(),
                    ClientCommitRevision::new(2),
                    [400.0, 200.0],
                    root([800.0, 500.0], 1.0)
                )
                .is_none()
        );
        assert!(matches!(
            sizing.observe(
                preferences(),
                ClientCommitRevision::new(3),
                [400.0, 200.0],
                root([400.0, 200.0], 1.0)
            ),
            Some(ClientSurfaceRequestKind::SetPreferredScale { .. })
        ));
    }

    #[test]
    fn framebuffer_size_waits_for_stability_and_stays_bounded() {
        let mut sizing = RasterSizing::default();
        let now = Instant::now();
        assert_eq!(sizing.update([1900, 1080], now), [1856, 1024]);
        assert_eq!(sizing.update([1000, 1800], now), [1856, 1024]);
        assert_eq!(
            sizing.update([1000, 1800], now + Duration::from_millis(299)),
            [1856, 1024]
        );
        assert_eq!(
            sizing.update([1000, 1800], now + Duration::from_millis(300)),
            [960, 1792]
        );
    }
}
