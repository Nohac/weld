//! Refresh-paced interactive-resize request coalescing.

use std::collections::HashMap;

use crate::surface::{Extent, SurfaceId};

#[derive(Default)]
pub(super) struct PendingResizeRequests(HashMap<SurfaceId, Extent>);

impl PendingResizeRequests {
    pub(super) fn queue(&mut self, surface: SurfaceId, logical_size: Extent) {
        self.0.insert(surface, logical_size);
    }

    pub(super) fn take(&mut self, surface: SurfaceId) -> Option<Extent> {
        self.0.remove(&surface)
    }

    pub(super) fn drain(&mut self) -> impl Iterator<Item = (SurfaceId, Extent)> + '_ {
        self.0.drain()
    }

    pub(super) fn discard(&mut self, surface: SurfaceId) {
        self.0.remove(&surface);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_latest_size_for_each_surface() {
        let mut requests = PendingResizeRequests::default();
        let first = SurfaceId::new(1);
        let second = SurfaceId::new(2);

        requests.queue(first, Extent::new(640, 480));
        requests.queue(second, Extent::new(800, 600));
        requests.queue(first, Extent::new(1280, 720));

        assert_eq!(requests.take(first), Some(Extent::new(1280, 720)));
        assert_eq!(requests.take(second), Some(Extent::new(800, 600)));
    }

    #[test]
    fn drain_clears_all_pending_requests() {
        let mut requests = PendingResizeRequests::default();
        requests.queue(SurfaceId::new(1), Extent::new(640, 480));
        requests.queue(SurfaceId::new(2), Extent::new(800, 600));

        assert_eq!(requests.drain().count(), 2);
        assert_eq!(requests.drain().count(), 0);
    }
}
