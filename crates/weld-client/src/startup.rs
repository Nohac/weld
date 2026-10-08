//! Initial presentation waits for requested geometry, with a latest-frame fallback
//! for applications that choose another size or stop committing after configure.
use crate::{ClientSurfaceRequestKind, Extent, ToplevelLayout};
use std::time::{Duration, Instant};

const INITIAL_SIZE_WAIT: Duration = Duration::from_secs(1);

/// A presenter's requested logical size and scale for a tiled application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowPreference {
    pub size: Extent,
    pub scale_120: u32,
}
impl WindowPreference {
    pub fn requests(self) -> [ClientSurfaceRequestKind; 2] {
        [
            ClientSurfaceRequestKind::SetPreferredScale {
                scale_120: Some(self.scale_120),
            },
            ClientSurfaceRequestKind::Configure {
                logical_size: self.size,
                layout: ToplevelLayout::Tiled,
                resizing: false,
                fullscreen: false,
            },
        ]
    }
}

pub struct InitialPresentation<T> {
    preference: Option<WindowPreference>,
    deadline: Option<Instant>,
    visible: bool,
    pending: Option<T>,
}

impl<T> Default for InitialPresentation<T> {
    fn default() -> Self {
        Self {
            preference: None,
            deadline: None,
            visible: false,
            pending: None,
        }
    }
}

impl<T> InitialPresentation<T> {
    pub fn configure(&mut self, preference: WindowPreference, now: Instant) {
        if self.preference != Some(preference) {
            self.preference = Some(preference);
            if !self.visible {
                self.deadline = Some(now + INITIAL_SIZE_WAIT);
            }
        }
    }

    pub fn offer(&mut self, frame: T, logical_size: [f32; 2], now: Instant) -> Option<T> {
        let fits = self.preference.is_some_and(|preference| {
            (logical_size[0] - preference.size.width as f32).abs() <= 1.0
                && (logical_size[1] - preference.size.height as f32).abs() <= 1.0
        });
        if self.visible || fits || self.deadline.is_some_and(|deadline| now >= deadline) {
            self.visible = true;
            self.deadline = None;
            self.pending = None;
            Some(frame)
        } else {
            self.pending = Some(frame);
            None
        }
    }

    pub fn poll(&mut self, now: Instant) -> Option<T> {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.deadline = None;
            if let Some(frame) = self.pending.take() {
                self.visible = true;
                return Some(frame);
            }
            // The next frame can also be an application's chosen size.
            self.visible = true;
        }
        None
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn is_waiting(&self) -> bool {
        !self.visible
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preference() -> WindowPreference {
        WindowPreference {
            size: Extent::new(448, 859),
            scale_120: 360,
        }
    }

    #[test]
    fn cached_desktop_frames_wait_for_phone_geometry_then_live_resizes_flow() {
        let now = Instant::now();
        let mut gate = InitialPresentation::default();
        assert!(gate.is_waiting());
        gate.configure(preference(), now);
        assert_eq!(gate.offer(1, [1920.0, 1080.0], now), None);
        assert_eq!(gate.offer(2, [1920.0, 1080.0], now), None);
        assert_eq!(gate.offer(3, [448.0, 859.0], now), Some(3));
        assert!(!gate.is_waiting());
        assert_eq!(gate.poll(now + INITIAL_SIZE_WAIT), None);
        let mut landscape = preference();
        landscape.size = Extent::new(940, 380);
        gate.configure(landscape, now);
        assert_eq!(gate.offer(4, [940.0, 380.0], now), Some(4));
        assert_eq!(gate.deadline(), None);
    }

    #[test]
    fn a_static_nonconforming_app_releases_only_the_latest_frame_at_deadline() {
        let now = Instant::now();
        let mut gate = InitialPresentation::default();
        gate.configure(preference(), now);
        assert_eq!(gate.offer(1, [500.0, 400.0], now), None);
        assert_eq!(gate.offer(2, [500.0, 400.0], now), None);
        assert_eq!(
            gate.poll(now + INITIAL_SIZE_WAIT - Duration::from_millis(1)),
            None
        );
        assert_eq!(gate.poll(now + INITIAL_SIZE_WAIT), Some(2));
        assert_eq!(gate.poll(now + INITIAL_SIZE_WAIT), None);
        assert_eq!(
            gate.offer(3, [500.0, 400.0], now + INITIAL_SIZE_WAIT),
            Some(3)
        );
    }

    #[test]
    fn rotation_before_first_presentation_replaces_the_pending_target() {
        let now = Instant::now();
        let mut gate = InitialPresentation::default();
        gate.configure(preference(), now);
        gate.offer(1, [900.0, 700.0], now);
        let mut landscape = preference();
        landscape.size = Extent::new(940, 380);
        gate.configure(landscape, now + Duration::from_millis(500));
        assert_eq!(gate.poll(now + INITIAL_SIZE_WAIT), None);
        assert_eq!(gate.offer(2, [448.0, 859.0], now + INITIAL_SIZE_WAIT), None);
        assert_eq!(
            gate.offer(3, [940.0, 380.0], now + INITIAL_SIZE_WAIT),
            Some(3)
        );
    }

    #[test]
    fn frame_before_viewport_is_kept_for_static_app_fallback() {
        let now = Instant::now();
        let mut gate = InitialPresentation::default();
        assert_eq!(gate.offer(1, [900.0, 700.0], now), None);
        gate.configure(preference(), now);
        assert_eq!(gate.poll(now + INITIAL_SIZE_WAIT), Some(1));
    }
}
