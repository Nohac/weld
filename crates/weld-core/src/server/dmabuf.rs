//! Smithay linux-dmabuf protocol state and client-buffer release ownership.

use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};

use smithay::{
    backend::allocator::{Buffer, Format, dmabuf::Dmabuf},
    reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_surface::WlSurface},
    reexports::wayland_server::{Resource, backend::ObjectId},
    wayland::dmabuf::{
        DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
    },
};
use tracing::{debug, warn};

use crate::{
    dmabuf::{DmabufCapabilities, DmabufReleaseId},
    server::ServerState,
};

pub(super) struct DmabufProtocol {
    pub(super) state: DmabufState,
    pub(super) _global: Option<DmabufGlobal>,
    formats: HashSet<Format>,
}

impl DmabufProtocol {
    pub(super) fn new(
        display: &smithay::reexports::wayland_server::DisplayHandle,
        capabilities: Option<&DmabufCapabilities>,
    ) -> anyhow::Result<Self> {
        let mut state = DmabufState::new();
        let (global, formats) = if let Some(capabilities) = capabilities {
            let feedback = DmabufFeedbackBuilder::new(
                capabilities.main_device,
                capabilities.formats.iter().copied(),
            )
            .build()?;
            let global =
                state.create_global_with_default_feedback::<ServerState>(display, &feedback);
            (Some(global), capabilities.formats.iter().copied().collect())
        } else {
            (None, HashSet::new())
        };
        Ok(Self {
            state,
            _global: global,
            formats,
        })
    }

    fn accepts(&self, dmabuf: &Dmabuf) -> bool {
        let supported_flags = smithay::backend::allocator::dmabuf::DmabufFlags::Y_INVERT;
        dmabuf.num_planes() == 1
            && dmabuf.size().w > 0
            && dmabuf.size().h > 0
            && (dmabuf.flags() - supported_flags).is_empty()
            && self.formats.contains(&dmabuf.format())
    }
}

struct ReleaseEntry {
    id: DmabufReleaseId,
    outstanding: usize,
}

struct ReleaseTracker<Key> {
    next: Option<u64>,
    by_key: HashMap<Key, ReleaseEntry>,
    key_by_id: HashMap<DmabufReleaseId, Key>,
}

impl<Key> Default for ReleaseTracker<Key> {
    fn default() -> Self {
        Self {
            next: Some(1),
            by_key: HashMap::new(),
            key_by_id: HashMap::new(),
        }
    }
}

impl<Key> ReleaseTracker<Key>
where
    Key: Clone + Eq + Hash,
{
    fn register(&mut self, key: Key) -> Option<DmabufReleaseId> {
        if let Some(entry) = self.by_key.get_mut(&key) {
            entry.outstanding = entry.outstanding.checked_add(1)?;
            return Some(entry.id);
        }
        let raw = self.next?;
        self.next = raw.checked_add(1);
        let id = DmabufReleaseId::new(raw);
        self.by_key
            .insert(key.clone(), ReleaseEntry { id, outstanding: 1 });
        self.key_by_id.insert(id, key);
        Some(id)
    }

    fn complete(&mut self, id: DmabufReleaseId) -> Option<Key> {
        let key = self.key_by_id.get(&id)?.clone();
        let entry = self.by_key.get_mut(&key)?;
        if entry.outstanding > 1 {
            entry.outstanding -= 1;
            return None;
        }
        self.by_key.remove(&key);
        self.key_by_id.remove(&id);
        Some(key)
    }

    fn destroyed(&mut self, key: &Key) {
        if let Some(entry) = self.by_key.remove(key) {
            self.key_by_id.remove(&entry.id);
        }
    }
}

#[derive(Default)]
pub(super) struct DmabufReleaseStore {
    tracker: ReleaseTracker<ObjectId>,
    buffers: HashMap<ObjectId, WlBuffer>,
}

impl DmabufReleaseStore {
    pub(super) fn register(&mut self, buffer: WlBuffer) -> Option<DmabufReleaseId> {
        let key = buffer.id();
        let id = self.tracker.register(key.clone())?;
        self.buffers.entry(key).or_insert(buffer);
        Some(id)
    }

    pub(super) fn complete(&mut self, id: DmabufReleaseId) {
        if let Some(key) = self.tracker.complete(id)
            && let Some(buffer) = self.buffers.remove(&key)
        {
            buffer.release();
            debug!(?id, "released client DMA-BUF after GPU completion");
        }
    }

    pub(super) fn destroyed(&mut self, destroyed: &WlBuffer) {
        let key = destroyed.id();
        self.tracker.destroyed(&key);
        self.buffers.remove(&key);
    }
}

impl DmabufHandler for ServerState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_protocol.state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if !self.dmabuf_protocol.accepts(&dmabuf) {
            warn!(
                format = ?dmabuf.format(),
                size = ?dmabuf.size(),
                planes = dmabuf.num_planes(),
                flags = ?dmabuf.flags(),
                "rejected unsupported client DMA-BUF"
            );
            notifier.failed();
            return;
        }
        if let Err(error) = self.dmabuf_sources.import(&dmabuf) {
            warn!(
                %error,
                format = ?dmabuf.format(),
                size = ?dmabuf.size(),
                planes = dmabuf.num_planes(),
                flags = ?dmabuf.flags(),
                strides = ?dmabuf.strides().collect::<Vec<_>>(),
                offsets = ?dmabuf.offsets().collect::<Vec<_>>(),
                "rejected client DMA-BUF that Vulkan could not import"
            );
            notifier.failed();
            return;
        }
        if let Err(error) = notifier.successful::<Self>() {
            self.dmabuf_sources.remove(&dmabuf);
            warn!(%error, "client disappeared while completing DMA-BUF creation");
        }
    }

    fn new_surface_feedback(
        &mut self,
        _surface: &WlSurface,
        _global: &DmabufGlobal,
    ) -> Option<smithay::wayland::dmabuf::DmabufFeedback> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::ReleaseTracker;

    #[test]
    fn duplicate_buffer_use_releases_only_after_every_gpu_use_completes() {
        let mut tracker = ReleaseTracker::default();
        let first = tracker
            .register(7_u32)
            .expect("identity should be available");
        let second = tracker.register(7_u32).expect("identity should be reused");

        assert_eq!(first, second);
        assert_eq!(tracker.complete(first), None);
        assert_eq!(tracker.complete(second), Some(7));
        assert_eq!(tracker.complete(second), None);
    }

    #[test]
    fn destruction_cancels_pending_release_completion() {
        let mut tracker = ReleaseTracker::default();
        let release = tracker
            .register(9_u32)
            .expect("identity should be available");

        tracker.destroyed(&9);

        assert_eq!(tracker.complete(release), None);
    }
}
