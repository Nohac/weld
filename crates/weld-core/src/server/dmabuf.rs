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
    wayland::drm_syncobj::{
        DrmSyncPoint, DrmSyncobjHandler, DrmSyncobjState, supports_syncobj_eventfd,
    },
};
use tracing::{debug, info, warn};

use crate::{
    dmabuf::{DmabufCapabilities, DmabufReleaseId},
    server::ServerState,
};

pub(super) struct DmabufProtocol {
    pub(super) state: DmabufState,
    pub(super) syncobj_state: Option<DrmSyncobjState>,
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
        let syncobj_state = capabilities
            .and_then(|capabilities| capabilities.syncobj_import_device.clone())
            .filter(supports_syncobj_eventfd)
            .map(|device| {
                info!("enabled linux-drm-syncobj explicit client synchronization");
                DrmSyncobjState::new::<ServerState>(display, device)
            });
        Ok(Self {
            state,
            syncobj_state,
            _global: global,
            formats,
        })
    }

    pub(super) const fn explicit_sync_enabled(&self) -> bool {
        self.syncobj_state.is_some()
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

struct BufferReleaseState {
    outstanding: usize,
    resource_alive: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct ReleaseCompletion<Key> {
    key: Key,
    final_use: bool,
    release_resource: bool,
}

struct ReleaseTracker<Key> {
    next: Option<u64>,
    by_key: HashMap<Key, BufferReleaseState>,
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
        let outstanding = self
            .by_key
            .get(&key)
            .map_or(Some(1), |entry| entry.outstanding.checked_add(1))?;
        let raw = self.next?;
        self.next = raw.checked_add(1);
        let id = DmabufReleaseId::new(raw);
        self.by_key
            .entry(key.clone())
            .and_modify(|entry| entry.outstanding = outstanding)
            .or_insert(BufferReleaseState {
                outstanding,
                resource_alive: true,
            });
        self.key_by_id.insert(id, key);
        Some(id)
    }

    fn complete(&mut self, id: DmabufReleaseId) -> Option<ReleaseCompletion<Key>> {
        let key = self.key_by_id.remove(&id)?;
        let entry = self.by_key.get_mut(&key)?;
        if entry.outstanding > 1 {
            entry.outstanding -= 1;
            return Some(ReleaseCompletion {
                key,
                final_use: false,
                release_resource: false,
            });
        }
        let release_resource = entry.resource_alive;
        self.by_key.remove(&key);
        Some(ReleaseCompletion {
            key,
            final_use: true,
            release_resource,
        })
    }

    fn destroyed(&mut self, key: &Key) {
        if let Some(entry) = self.by_key.get_mut(key) {
            entry.resource_alive = false;
        }
    }
}

#[derive(Default)]
pub(super) struct DmabufReleaseStore {
    tracker: ReleaseTracker<ObjectId>,
    buffers: HashMap<ObjectId, WlBuffer>,
    release_points: HashMap<DmabufReleaseId, DrmSyncPoint>,
}

impl DmabufReleaseStore {
    pub(super) fn register(
        &mut self,
        buffer: WlBuffer,
        release_point: Option<DrmSyncPoint>,
    ) -> Option<DmabufReleaseId> {
        let key = buffer.id();
        let id = self.tracker.register(key.clone())?;
        self.buffers.entry(key).or_insert(buffer);
        if let Some(release_point) = release_point {
            self.release_points.insert(id, release_point);
        }
        Some(id)
    }

    pub(super) fn complete(&mut self, id: DmabufReleaseId) {
        signal_release_point(
            self.release_points.remove(&id),
            "client DMA-BUF GPU completion",
        );
        let Some(completion) = self.tracker.complete(id) else {
            return;
        };
        if completion.final_use {
            let buffer = self.buffers.remove(&completion.key);
            if completion.release_resource
                && let Some(buffer) = buffer
            {
                buffer.release();
            }
        }
        debug!(
            ?id,
            final_use = completion.final_use,
            "completed a client DMA-BUF use"
        );
    }

    pub(super) fn destroyed(&mut self, destroyed: &WlBuffer) {
        let key = destroyed.id();
        self.tracker.destroyed(&key);
        self.buffers.remove(&key);
    }
}

pub(super) fn signal_release_point(release_point: Option<DrmSyncPoint>, reason: &'static str) {
    let Some(release_point) = release_point else {
        return;
    };
    if let Err(error) = release_point.signal() {
        warn!(%error, reason, "failed to signal a client DMA-BUF release point");
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

impl DrmSyncobjHandler for ServerState {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.dmabuf_protocol.syncobj_state.as_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::ReleaseTracker;

    #[test]
    fn duplicate_buffer_uses_have_independent_completion_identities() {
        let mut tracker = ReleaseTracker::default();
        let first = tracker
            .register(7_u32)
            .expect("identity should be available");
        let second = tracker
            .register(7_u32)
            .expect("second identity should be available");

        assert_ne!(first, second);
        assert_eq!(
            tracker.complete(first),
            Some(super::ReleaseCompletion {
                key: 7,
                final_use: false,
                release_resource: false,
            })
        );
        assert_eq!(
            tracker.complete(second),
            Some(super::ReleaseCompletion {
                key: 7,
                final_use: true,
                release_resource: true,
            })
        );
        assert_eq!(tracker.complete(second), None);
    }

    #[test]
    fn destruction_preserves_pending_use_completion_without_resource_release() {
        let mut tracker = ReleaseTracker::default();
        let first = tracker
            .register(9_u32)
            .expect("identity should be available");
        let second = tracker
            .register(9_u32)
            .expect("second identity should be available");

        tracker.destroyed(&9);

        assert_eq!(
            tracker.complete(first),
            Some(super::ReleaseCompletion {
                key: 9,
                final_use: false,
                release_resource: false,
            })
        );
        assert_eq!(
            tracker.complete(second),
            Some(super::ReleaseCompletion {
                key: 9,
                final_use: true,
                release_resource: false,
            })
        );
    }
}
