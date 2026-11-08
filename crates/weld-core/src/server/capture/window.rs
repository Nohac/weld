//! Foreign window identities and on-demand views shared with phone streaming.

use super::{CaptureTarget, FrameWork};
use crate::{
    WAYLAND_CLIENT_SOURCE,
    dmabuf::{
        DmabufCapabilities,
        composition::image_capture::{ImageCaptureTask, WindowImageCapture},
    },
    renderer::capture_task::CaptureWorker,
    server::ServerState,
    surface::{Extent, SurfaceId},
};
use anyhow::{Context, Result};
use smithay::{
    reexports::wayland_server::DisplayHandle,
    wayland::{
        foreign_toplevel_list::{
            ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState,
        },
        image_capture_source::{
            ImageCaptureSource, ToplevelCaptureSourceHandler, ToplevelCaptureSourceState,
        },
        image_copy_capture::{CaptureFailureReason, ImageCopyCaptureHandler},
    },
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};
use weld_client::{
    ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceMetadata, ClientSurfaceRole,
    ComposedBuffer, SourceViews, SurfaceBufferChange, SurfaceStreamMode,
};

pub(super) struct WindowSources {
    list: ForeignToplevelListState,
    sources: ToplevelCaptureSourceState,
    handles: HashMap<SurfaceId, ForeignToplevelHandle>,
    metadata: HashMap<SurfaceId, ClientSurfaceMetadata>,
    members: HashSet<SurfaceId>,
    demand: HashSet<SurfaceId>,
    views: SourceViews,
    render_node: Option<PathBuf>,
    renderer: Option<WindowImageCapture>,
}

impl WindowSources {
    pub(super) fn new(display: &DisplayHandle, capabilities: Option<&DmabufCapabilities>) -> Self {
        let render_node = capabilities
            .and_then(|caps| caps.external_imports().ok())
            .map(|caps| caps.render_node);
        let supported = render_node.is_some();
        Self {
            list: ForeignToplevelListState::new::<ServerState>(display),
            sources: ToplevelCaptureSourceState::new_with_filter::<ServerState, _>(
                display,
                move |_| supported,
            ),
            handles: HashMap::new(),
            metadata: HashMap::new(),
            members: HashSet::new(),
            demand: HashSet::new(),
            views: SourceViews::new(SurfaceStreamMode::Composited),
            render_node,
            renderer: None,
        }
    }
    pub(super) fn extent(&self, surface: SurfaceId) -> Option<Extent> {
        self.handles.get(&surface)?;
        let geometry = self.views.viewport(surface)?;
        Some(Extent::new(
            geometry.view.source_width.ceil() as u32,
            geometry.view.source_height.ceil() as u32,
        ))
    }
    fn scene(&mut self, surface: SurfaceId) -> Result<ComposedBuffer> {
        let (_, event) = self
            .views
            .compose(surface)?
            .context("window has no current scene")?;
        let ClientSurfaceEventKind::Commit(commit) = event.kind else {
            anyhow::bail!("window scene is not a commit");
        };
        commit
            .buffers
            .iter()
            .find_map(|update| match &update.change {
                SurfaceBufferChange::Replaced { buffer, .. } => {
                    buffer.access::<ComposedBuffer>().cloned()
                }
                _ => None,
            })
            .context("window is not mapped")
    }
}

impl ForeignToplevelListHandler for ServerState {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.capture.windows.list
    }
}
impl ToplevelCaptureSourceHandler for ServerState {
    fn toplevel_capture_source_state(&mut self) -> &mut ToplevelCaptureSourceState {
        &mut self.capture.windows.sources
    }
    fn toplevel_source_created(
        &mut self,
        source: ImageCaptureSource,
        toplevel: ForeignToplevelHandle,
    ) {
        if !toplevel.is_closed()
            && let Some(surface) = toplevel.user_data().get::<SurfaceId>()
        {
            source.user_data().insert_if_missing(|| *surface);
            source.user_data().insert_if_missing(|| toplevel);
        }
    }
}

impl ServerState {
    pub(crate) fn observe_capture_event(&mut self, event: &ClientSurfaceEvent) {
        if event.surface.source() != WAYLAND_CLIENT_SOURCE {
            return;
        }
        let windows = &mut self.capture.windows;
        let id = event.surface;
        match &event.kind {
            ClientSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(_))
            | ClientSurfaceEventKind::Role(ClientSurfaceRole::Popup(_)) => {
                windows.members.insert(id);
            }
            ClientSurfaceEventKind::Destroyed => {
                windows.members.remove(&id);
                windows.metadata.remove(&id);
                if let Some(handle) = windows.handles.remove(&id) {
                    handle.send_closed();
                    windows.list.cleanup_closed_handles();
                }
            }
            ClientSurfaceEventKind::Metadata(metadata) => {
                windows.metadata.insert(id, metadata.clone());
            }
            _ => {}
        }
        if !windows.members.contains(&id)
            && !matches!(event.kind, ClientSurfaceEventKind::Destroyed)
        {
            return;
        }
        if let Err(error) = windows.views.retain((), event.clone()) {
            tracing::warn!(%error, "could not retain capture scene");
        }
        if windows.views.root(id) == Some(id) && windows.views.viewport(id).is_some() {
            let empty = ClientSurfaceMetadata::default();
            let metadata = windows.metadata.get(&id).unwrap_or(&empty);
            if let Some(handle) = windows.handles.get(&id) {
                if matches!(event.kind, ClientSurfaceEventKind::Metadata(_)) {
                    handle.send_title(metadata.title());
                    handle.send_app_id(metadata.app_id());
                    handle.send_done();
                }
            } else {
                let handle = windows
                    .list
                    .new_toplevel::<Self>(metadata.title(), metadata.app_id());
                handle.user_data().insert_if_missing(|| id);
                windows.handles.insert(id, handle);
            }
        } else if let Some(handle) = windows.handles.remove(&id) {
            handle.send_closed();
            windows.list.cleanup_closed_handles();
        }
    }

    pub(super) fn refresh_capture_demand(&mut self) {
        // A session may remain open while its recorder is paused. Only admitted
        // frame requests keep hidden applications producing new content.
        let roots: HashSet<_> = self
            .capture
            .pending
            .iter()
            .filter_map(|pending| match pending.target {
                CaptureTarget::Window(id) => Some(id),
                CaptureTarget::Output(_) => None,
            })
            .collect();
        if roots.is_empty() {
            let previous = std::mem::take(&mut self.capture.windows.demand);
            for surface in previous {
                self.set_capture_presentation(surface, false);
            }
            return;
        }
        let next: HashSet<_> = self
            .capture
            .windows
            .members
            .iter()
            .copied()
            .filter(|id| {
                self.capture
                    .windows
                    .views
                    .root(*id)
                    .is_some_and(|root| roots.contains(&root))
            })
            .collect();
        let previous = std::mem::replace(&mut self.capture.windows.demand, next.clone());
        for surface in previous.difference(&next) {
            self.set_capture_presentation(*surface, false);
        }
        for surface in next.difference(&previous) {
            self.set_capture_presentation(*surface, true);
        }
    }

    pub(crate) fn capture_windows(&mut self, worker: &CaptureWorker) {
        if let Some(renderer) = &mut self.capture.windows.renderer {
            renderer.prune();
        }
        if self
            .capture
            .windows
            .renderer
            .as_ref()
            .is_some_and(WindowImageCapture::is_idle)
            && !self.capture.sessions.iter().any(|session| {
                matches!(
                    CaptureTarget::of(&session.source()),
                    Some(CaptureTarget::Window(_))
                )
            })
        {
            self.capture.windows.renderer = None;
        }
        if self.session_locked() {
            return;
        }
        let requests: Vec<_> = self
            .capture
            .pending
            .iter()
            .enumerate()
            .filter_map(|(index, pending)| match pending.target {
                CaptureTarget::Window(id)
                    if matches!(pending.work, FrameWork::Waiting)
                        && !pending.frame.has_failed()
                        && pending
                            .writable
                            .as_ref()
                            .is_none_or(|fence| fence.ready.get()) =>
                {
                    Some((index, id))
                }
                _ => None,
            })
            .collect();
        for (index, id) in requests {
            let source = self.capture.pending[index].session.source();
            let extent = self.capture.pending[index].extent;
            let Some(constraints) = self.capture_constraints(&source) else {
                self.capture.pending[index].work = FrameWork::Failed(CaptureFailureReason::Stopped);
                continue;
            };
            if (constraints.size.w as u32, constraints.size.h as u32)
                != (extent.width, extent.height)
            {
                self.capture.pending[index].work =
                    FrameWork::Failed(CaptureFailureReason::BufferConstraints);
                continue;
            }
            let result = (|| -> Result<ImageCaptureTask> {
                let mut scene = self.capture.windows.scene(id)?;
                if self.capture.pending[index].session.draw_cursor()
                    && let Some(hit) = self.local_input.pointer_target.get()
                    && let Some(origin) =
                        self.capture
                            .windows
                            .views
                            .input_origin(id, hit.surface, hit.layer)
                {
                    scene.cursor = Some(weld_client::CompositionCursor {
                        image: self.presented_cursor.clone().unwrap_or_default(),
                        position: weld_client::LogicalPoint::new(
                            origin.x + hit.local_position.x as f32,
                            origin.y + hit.local_position.y as f32,
                        ),
                    });
                }
                anyhow::ensure!(
                    scene.extent == self.capture.pending[index].extent,
                    "window capture size changed"
                );
                if self.capture.windows.renderer.is_none() {
                    let path = self
                        .capture
                        .windows
                        .render_node
                        .as_ref()
                        .context("window capture requires a render node")?;
                    self.capture.windows.renderer =
                        Some(WindowImageCapture::new(path, worker.notifier())?);
                }
                let buffer = smithay::wayland::dmabuf::get_dmabuf(
                    &self.capture.pending[index].frame.buffer(),
                )
                .ok()
                .cloned();
                self.capture
                    .windows
                    .renderer
                    .as_mut()
                    .context("window renderer unavailable")?
                    .submit(&scene, buffer.as_ref(), worker)
            })();
            let pending = &mut self.capture.pending[index];
            pending.writable = None;
            pending.sampled_at = smithay::utils::Clock::<smithay::utils::Monotonic>::new()
                .now()
                .into();
            pending.work = match result {
                Ok(ImageCaptureTask::Gpu(task)) => FrameWork::Gpu(task),
                Ok(ImageCaptureTask::Readback(task)) => FrameWork::Readback(task),
                Err(error) => {
                    tracing::warn!(%error, "could not capture window");
                    FrameWork::Failed(CaptureFailureReason::Unknown)
                }
            };
        }
    }
}
