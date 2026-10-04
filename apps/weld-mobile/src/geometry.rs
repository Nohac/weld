use weld_client::Extent;

const STATUS_HEIGHT: f64 = 64.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowPreference {
    pub size: Extent,
    pub scale_120: u32,
}

/// Logical presentation coordinates derived from Android's physical content area.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Viewport {
    pub video: [f64; 4],
    pub status: [f64; 4],
    pub preference: WindowPreference,
}

impl Viewport {
    pub fn new(window: [u32; 2], content: [i32; 4], density: f64) -> Option<Self> {
        if window.contains(&0) || !density.is_finite() || !(0.5..=8.0).contains(&density) {
            return None;
        }
        // Empty rectangles precede Android's first inset notification.
        let [left, top, right, bottom] = content.map(f64::from);
        let left = left.clamp(0.0, f64::from(window[0]));
        let top = top.clamp(0.0, f64::from(window[1]));
        let width = right.min(f64::from(window[0])) - left;
        let height = bottom.min(f64::from(window[1])) - top;
        if width < density || height <= STATUS_HEIGHT * density {
            return None;
        }
        let video = [
            left / density,
            top / density,
            width / density,
            height / density - STATUS_HEIGHT,
        ];
        let scale_120 = (density * 120.0).round() as u32;
        // Reserve room for decorations and legacy integer-scale buffers within
        // the receiver's 4096-pixel dimension ceiling.
        let max_logical = 3840.0 / f64::from(scale_120.div_ceil(120));
        let reduction = (max_logical / video[2].max(video[3])).min(1.0);
        Some(Self {
            video,
            status: [video[0], video[1] + video[3], video[2], STATUS_HEIGHT],
            preference: WindowPreference {
                size: Extent::new(
                    (video[2] * reduction).floor().max(1.0) as u32,
                    (video[3] * reduction).floor().max(1.0) as u32,
                ),
                scale_120,
            },
        })
    }
}

/// Fit committed content into the available logical rectangle.
pub(crate) fn fit_rect(area: [f64; 4], content: [f64; 2]) -> [f64; 4] {
    if area.into_iter().chain(content).any(|v| !v.is_finite())
        || area[2..].iter().chain(content.iter()).any(|v| *v <= 0.0)
    {
        return [0.0; 4];
    }
    let scale = (area[2] / content[0]).min(area[3] / content[1]);
    let width = content[0] * scale;
    let height = content[1] * scale;
    [
        area[0] + (area[2] - width) / 2.0,
        area[1] + (area[3] - height) / 2.0,
        width,
        height,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn density_insets_and_status_define_requested_size() {
        let viewport = Viewport::new([1344, 2992], [0, 96, 1344, 2928], 3.0).expect("viewport");
        assert_eq!(viewport.video, [0.0, 32.0, 448.0, 880.0]);
        assert_eq!(viewport.status, [0.0, 912.0, 448.0, 64.0]);
        assert_eq!(
            viewport.preference,
            WindowPreference {
                size: Extent::new(448, 880),
                scale_120: 360
            }
        );
        let landscape = Viewport::new([2992, 1344], [96, 0, 2928, 1344], 3.0).expect("landscape");
        assert_eq!(landscape.preference.size, Extent::new(944, 384));
    }

    #[test]
    fn invalid_insets_wait_and_large_displays_bound_integer_scale_buffers() {
        let low_density = Viewport::new([480, 800], [0, 0, 480, 800], 0.75).expect("ldpi");
        assert_eq!(low_density.preference.scale_120, 90);
        assert!(Viewport::new([1344, 2992], [0; 4], 3.0).is_none());
        assert!(Viewport::new([1344, 2992], [0, 0, 1344, 2992], f64::NAN).is_none());
        let viewport = Viewport::new([8000, 4000], [0, 0, 8000, 4000], 2.5).expect("viewport");
        let size = viewport.preference.size;
        assert!(size.width * 3 <= 3840 && size.height * 3 <= 3840);
    }

    #[test]
    fn portrait_and_landscape_fit_leave_status_outside_input() {
        assert_eq!(
            fit_rect([20.0, 30.0, 400.0, 800.0], [800.0, 400.0]),
            [20.0, 330.0, 400.0, 200.0]
        );
        assert_eq!(
            fit_rect([0.0, 0.0, 800.0, 400.0], [400.0, 800.0]),
            [300.0, 0.0, 200.0, 400.0]
        );
        assert_eq!(fit_rect([0.0; 4], [800.0, 400.0]), [0.0; 4]);
        assert_eq!(
            fit_rect([0.0, 0.0, 400.0, 800.0], [f64::NAN, 400.0]),
            [0.0; 4]
        );
    }
}
