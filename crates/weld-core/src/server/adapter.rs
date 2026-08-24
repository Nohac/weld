//! Same-thread adapter boundary between Smithay dispatch and neutral clients.

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::Rc,
};

use tracing::warn;
use weld_client::{
    ClientAdapter, ClientAdapterCommandEnvelope, ClientAdapterRegistration, ClientBufferId,
    ClientBufferLease, ClientBufferMetadata, ClientBufferUseId, ClientCommitRevision,
    ClientEventQueue, ClientInputEvent, ClientProvenance, ClientRequest, ClientSourceDescriptor,
    ClientSurfaceCommit, ClientSurfaceEvent, ClientSurfaceEventKind, SurfaceBufferChange,
    SurfaceBufferUpdate,
};

use crate::{
    WAYLAND_CLIENT_SOURCE,
    dmabuf::{DirectClientBufferAccess, DmabufContext, WaylandShmBuffer},
};

use super::{
    PendingSurfaceBufferContent, PendingSurfaceEvent, PendingSurfaceEventKind,
    PendingSurfaceTreeSnapshot,
};

#[derive(Default)]
struct WaylandClientBridgeState {
    events: VecDeque<PendingSurfaceEvent>,
    work: VecDeque<WaylandClientWork>,
}

/// Shared same-thread mailbox used because Smithay owns its concrete dispatch state.
#[derive(Clone, Default)]
pub(crate) struct WaylandClientBridge(Rc<RefCell<WaylandClientBridgeState>>);

impl WaylandClientBridge {
    pub(crate) fn push_back(&self, event: PendingSurfaceEvent) {
        self.0.borrow_mut().events.push_back(event);
    }

    fn pop_event(&self) -> Option<PendingSurfaceEvent> {
        self.0.borrow_mut().events.pop_front()
    }

    fn push_work(&self, work: WaylandClientWork) {
        self.0.borrow_mut().work.push_back(work);
    }

    pub(crate) fn pop_work(&self) -> Option<WaylandClientWork> {
        self.0.borrow_mut().work.pop_front()
    }
}

pub(crate) enum WaylandClientWork {
    Request(ClientRequest),
    Input(ClientInputEvent),
    HostFocusLost(u32),
}

/// Marker paired with the built-in adapter for application-side buffer import.
#[derive(Debug)]
pub struct WaylandClientImporter;

pub(crate) fn registration(
    bridge: WaylandClientBridge,
    dmabuf: DmabufContext,
) -> ClientAdapterRegistration {
    let descriptor = ClientSourceDescriptor::new(WAYLAND_CLIENT_SOURCE, ClientProvenance::Local);
    ClientAdapterRegistration::new(
        descriptor,
        WaylandClientAdapter::new(bridge, dmabuf),
        WaylandClientImporter,
    )
}

struct WaylandClientAdapter {
    bridge: WaylandClientBridge,
    dmabuf: DmabufContext,
    revisions: HashMap<crate::surface::SurfaceId, u64>,
    buffer_ids: WaylandBufferIds,
    next_buffer_use: Option<u64>,
}

struct WaylandBufferIds {
    dmabufs: HashMap<crate::dmabuf::ImportId, u64>,
    next: Option<u64>,
}

impl Default for WaylandBufferIds {
    fn default() -> Self {
        Self {
            dmabufs: HashMap::new(),
            next: Some(1),
        }
    }
}

impl WaylandBufferIds {
    fn allocate(&mut self) -> Option<u64> {
        let current = self.next?;
        self.next = current.checked_add(1);
        Some(current)
    }

    fn dmabuf(&mut self, import: crate::dmabuf::ImportId) -> Option<u64> {
        if let Some(local) = self.dmabufs.get(&import) {
            return Some(*local);
        }
        let local = self.allocate()?;
        self.dmabufs.insert(import, local);
        Some(local)
    }
}

impl WaylandClientAdapter {
    fn new(bridge: WaylandClientBridge, dmabuf: DmabufContext) -> Self {
        Self {
            bridge,
            dmabuf,
            revisions: HashMap::new(),
            buffer_ids: WaylandBufferIds::default(),
            next_buffer_use: Some(1),
        }
    }

    fn translate_event(&mut self, event: PendingSurfaceEvent) -> Option<ClientSurfaceEvent> {
        if matches!(&event.kind, PendingSurfaceEventKind::Destroyed) {
            self.revisions.remove(&event.surface);
        }
        if !matches!(&event.kind, PendingSurfaceEventKind::TreeSnapshot(_)) {
            return translate_non_commit_event(event);
        }
        let PendingSurfaceEvent { surface, kind } = event;
        let kind = match kind {
            PendingSurfaceEventKind::TreeSnapshot(snapshot) => {
                ClientSurfaceEventKind::Commit(self.translate_commit(surface, snapshot))
            }
            PendingSurfaceEventKind::Role(_)
            | PendingSurfaceEventKind::WindowInteraction(_)
            | PendingSurfaceEventKind::Destroyed => return None,
        };
        Some(ClientSurfaceEvent { surface, kind })
    }

    fn translate_commit(
        &mut self,
        surface: crate::surface::SurfaceId,
        snapshot: PendingSurfaceTreeSnapshot,
    ) -> ClientSurfaceCommit {
        let revision = {
            let revision = self.revisions.entry(surface).or_default();
            *revision = revision.saturating_add(1);
            *revision
        };
        let PendingSurfaceTreeSnapshot {
            client_mapped,
            root,
            window_geometry,
            overlays,
            inputs,
            buffers,
        } = snapshot;
        let buffers = buffers
            .into_iter()
            .map(|buffer| {
                let metadata = ClientBufferMetadata::new(
                    weld_client::Extent::new(buffer.width, buffer.height),
                    buffer.opaque,
                );
                let change = match buffer.content {
                    PendingSurfaceBufferContent::Retained => {
                        SurfaceBufferChange::Retained { metadata }
                    }
                    PendingSurfaceBufferContent::ShmPixels(bgra_pixels) => {
                        let Some(use_local) = self.allocate_buffer_use() else {
                            warn!(?surface, layer = ?buffer.layer, "discarded SHM content because client-buffer use identity space is exhausted");
                            return SurfaceBufferUpdate {
                                layer: buffer.layer,
                                change: SurfaceBufferChange::Retained { metadata },
                            };
                        };
                        let Some(buffer_local) = self.buffer_ids.allocate() else {
                            warn!(?surface, layer = ?buffer.layer, "discarded SHM content because client-buffer identity space is exhausted");
                            return SurfaceBufferUpdate {
                                layer: buffer.layer,
                                change: SurfaceBufferChange::Retained { metadata },
                            };
                        };
                        let lease = ClientBufferLease::new(
                            ClientBufferId::new(WAYLAND_CLIENT_SOURCE, buffer_local),
                            ClientBufferUseId::new(WAYLAND_CLIENT_SOURCE, use_local),
                            metadata,
                            Rc::new(DirectClientBufferAccess::Shm(WaylandShmBuffer {
                                bgra_pixels,
                            })),
                            |_| {},
                        );
                        match lease {
                            Ok(buffer) => SurfaceBufferChange::Replaced { metadata, buffer },
                            Err(error) => {
                                warn!(%error, ?surface, layer = ?buffer.layer, "rejected SHM client-buffer lease");
                                SurfaceBufferChange::Retained { metadata }
                            }
                        }
                    }
                    PendingSurfaceBufferContent::ImportedDmabuf(frame) => {
                        let Some(import_id) = self.dmabuf.import_id(&frame) else {
                            self.dmabuf.release_unrendered(frame);
                            warn!(?surface, layer = ?buffer.layer, "discarded DMA-BUF content because its protocol import is unavailable");
                            return SurfaceBufferUpdate {
                                layer: buffer.layer,
                                change: SurfaceBufferChange::Retained { metadata },
                            };
                        };
                        let Some(buffer_local) = self.buffer_ids.dmabuf(import_id) else {
                            self.dmabuf.release_unrendered(frame);
                            warn!(?surface, layer = ?buffer.layer, "discarded DMA-BUF content because client-buffer identity space is exhausted");
                            return SurfaceBufferUpdate {
                                layer: buffer.layer,
                                change: SurfaceBufferChange::Retained { metadata },
                            };
                        };
                        let Some(use_local) = self.allocate_buffer_use() else {
                            self.dmabuf.release_unrendered(frame);
                            warn!(?surface, layer = ?buffer.layer, "discarded DMA-BUF content because client-buffer use identity space is exhausted");
                            return SurfaceBufferUpdate {
                                layer: buffer.layer,
                                change: SurfaceBufferChange::Retained { metadata },
                            };
                        };
                        match self.dmabuf.lease_dmabuf(
                            WAYLAND_CLIENT_SOURCE,
                            buffer_local,
                            use_local,
                            metadata,
                            frame,
                        ) {
                            Ok(buffer) => SurfaceBufferChange::Replaced { metadata, buffer },
                            Err(error) => {
                                warn!(%error, ?surface, layer = ?buffer.layer, "rejected DMA-BUF client-buffer lease");
                                SurfaceBufferChange::Retained { metadata }
                            }
                        }
                    }
                };
                SurfaceBufferUpdate {
                    layer: buffer.layer,
                    change,
                }
            })
            .collect();
        ClientSurfaceCommit {
            revision: ClientCommitRevision::new(revision),
            mapped: client_mapped,
            root,
            window_geometry,
            overlays,
            inputs,
            buffers,
        }
    }

    fn allocate_buffer_use(&mut self) -> Option<u64> {
        let current = self.next_buffer_use?;
        self.next_buffer_use = current.checked_add(1);
        Some(current)
    }
}

fn translate_non_commit_event(event: PendingSurfaceEvent) -> Option<ClientSurfaceEvent> {
    let PendingSurfaceEvent { surface, kind } = event;
    let kind = match kind {
        PendingSurfaceEventKind::Role(role) => ClientSurfaceEventKind::Role(role),
        PendingSurfaceEventKind::WindowInteraction(interaction) => {
            ClientSurfaceEventKind::Interaction(interaction)
        }
        PendingSurfaceEventKind::Destroyed => ClientSurfaceEventKind::Destroyed,
        PendingSurfaceEventKind::TreeSnapshot(_) => return None,
    };
    Some(ClientSurfaceEvent { surface, kind })
}

impl ClientAdapter for WaylandClientAdapter {
    fn drain_events(&mut self, events: &mut ClientEventQueue) {
        while let Some(event) = self.bridge.pop_event() {
            if let Some(event) = self.translate_event(event) {
                events.push(event);
            }
        }
    }

    fn apply_request(&mut self, request: ClientRequest) {
        self.bridge.push_work(WaylandClientWork::Request(request));
    }

    fn apply_input(&mut self, event: ClientInputEvent) {
        self.bridge.push_work(WaylandClientWork::Input(event));
    }

    fn apply_command(&mut self, _command: ClientAdapterCommandEnvelope) {
        warn!("ignored an unsupported command for the Wayland client adapter");
    }

    fn host_focus_lost(&mut self, time: u32) {
        self.bridge
            .push_work(WaylandClientWork::HostFocusLost(time));
    }
}

#[cfg(test)]
mod tests {
    use weld_client::{ClientSurfaceEventKind, ClientSurfaceRole, ToplevelState, WindowDecoration};

    use super::{
        PendingSurfaceEvent, PendingSurfaceEventKind, WaylandBufferIds, translate_non_commit_event,
    };
    use crate::surface::SurfaceId;

    #[test]
    fn bridge_translation_preserves_identity_and_atomic_toplevel_role() {
        let surface = SurfaceId::for_test(7);
        let role = ClientSurfaceRole::Toplevel(ToplevelState {
            parent: Some(SurfaceId::for_test(3)),
            decoration: WindowDecoration::ServerSide,
        });

        let translated = translate_non_commit_event(PendingSurfaceEvent {
            surface,
            kind: PendingSurfaceEventKind::Role(role),
        })
        .expect("role events do not require buffer importer state");

        assert_eq!(translated.surface, surface);
        assert!(matches!(
            translated.kind,
            ClientSurfaceEventKind::Role(translated_role) if translated_role == role
        ));
    }

    #[test]
    fn wayland_buffer_ids_share_one_namespace_and_reuse_dmabuf_allocations() {
        let mut ids = WaylandBufferIds::default();
        let shm = ids.allocate().expect("SHM buffer ID");
        let dmabuf = ids
            .dmabuf(crate::dmabuf::ImportId::for_test(1))
            .expect("DMA-BUF buffer ID");

        assert_ne!(shm, dmabuf);
        assert_eq!(
            ids.dmabuf(crate::dmabuf::ImportId::for_test(1)),
            Some(dmabuf)
        );
    }
}
