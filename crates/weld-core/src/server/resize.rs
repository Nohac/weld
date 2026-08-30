//! Interactive-resize request coalescing at the client-adapter boundary.

use std::collections::HashMap;

use crate::surface::{Extent, SurfaceId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PendingResize {
    pub(super) logical_size: Extent,
    pub(super) resizing: bool,
}

#[derive(Default)]
pub(super) struct PendingResizeRequests(HashMap<SurfaceId, PendingResize>);

impl PendingResizeRequests {
    pub(super) fn queue(&mut self, surface: SurfaceId, request: PendingResize) {
        self.0.insert(surface, request);
    }

    pub(super) fn take(&mut self, surface: SurfaceId) -> Option<PendingResize> {
        self.0.remove(&surface)
    }

    pub(super) fn drain(&mut self) -> impl Iterator<Item = (SurfaceId, PendingResize)> + '_ {
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
        let first = SurfaceId::for_test(1);
        let second = SurfaceId::for_test(2);

        requests.queue(first, request(640, 480, true));
        requests.queue(second, request(800, 600, true));
        requests.queue(first, request(1280, 720, false));

        assert_eq!(requests.take(first), Some(request(1280, 720, false)));
        assert_eq!(requests.take(second), Some(request(800, 600, true)));
    }

    #[test]
    fn drain_clears_all_pending_requests() {
        let mut requests = PendingResizeRequests::default();
        requests.queue(SurfaceId::for_test(1), request(640, 480, true));
        requests.queue(SurfaceId::for_test(2), request(800, 600, false));

        assert_eq!(requests.drain().count(), 2);
        assert_eq!(requests.drain().count(), 0);
    }

    const fn request(width: u32, height: u32, resizing: bool) -> PendingResize {
        PendingResize {
            logical_size: Extent::new(width, height),
            resizing,
        }
    }
}
