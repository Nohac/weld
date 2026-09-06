//! Wayland cursor intent and client cursor-surface lifetime handling.

use std::{collections::HashMap, sync::Arc};

use smithay::{
    input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData},
    reexports::wayland_server::{Resource, backend::ObjectId, protocol::wl_surface::WlSurface},
    wayland::{
        compositor::{BufferAssignment, SurfaceAttributes, get_role, with_states},
        dmabuf::get_dmabuf,
        drm_syncobj::DrmSyncobjCachedState,
        seat::CURSOR_IMAGE_ROLE,
    },
};
use tracing::warn;
use weld_client::{ClientCursorUpdate, ClientRuntime};

use crate::cursor::{CursorAppearance, CursorImage, raster::canonical_cursor, unpremultiply_alpha};

use super::{
    ServerState,
    dmabuf::signal_release_point,
    shm::{SurfaceBufferMetadata, checked_buffer_scale, copy_shm_buffer, surface_content_view},
};

#[derive(Default)]
pub(super) struct CursorSurfaceStore {
    surfaces: HashMap<ObjectId, CachedCursorSurface>,
}

enum CachedCursorSurface {
    Empty,
    Unsupported,
    Image {
        pixels: Arc<[u8]>,
        metadata: SurfaceBufferMetadata,
        view: crate::surface::SurfaceContentView,
    },
}

impl ServerState {
    pub(crate) fn set_shell_cursor(&mut self, appearance: CursorAppearance) {
        self.shell_cursor = appearance;
    }

    pub(crate) fn set_shell_cursor_override(&mut self, active: bool) {
        self.shell_cursor_override = active;
    }

    pub(super) fn set_shell_cursor_ownership(&mut self, owned: bool) {
        if self.shell_owns_cursor == owned {
            return;
        }
        self.shell_owns_cursor = owned;
        if !owned {
            self.cursor_feedback_dirty = true;
            let default = CursorImageStatus::default_named();
            if self.cursor_status != default {
                self.cursor_status = default;
            }
        }
    }

    pub(super) fn commit_cursor_surface(&mut self, surface: &WlSurface) -> bool {
        if get_role(surface) != Some(CURSOR_IMAGE_ROLE) {
            return false;
        }
        self.refresh_cursor_surface(surface);
        true
    }

    pub(super) fn remove_cursor_surface(&mut self, surface: &WlSurface) {
        self.cursor_surfaces.surfaces.remove(&surface.id());
        if matches!(&self.cursor_status, CursorImageStatus::Surface(current) if current == surface)
        {
            self.cursor_status = CursorImageStatus::default_named();
            self.cursor_feedback_dirty = true;
        }
    }

    pub(crate) fn take_cursor_image(&mut self, clients: &ClientRuntime) -> Option<CursorImage> {
        let selected = select_cursor(
            self.shell_cursor,
            self.shell_cursor_override,
            self.shell_owns_cursor,
            clients.pointer_cursor(),
        );
        if self.presented_cursor.as_ref() == Some(&selected) {
            return None;
        }
        self.presented_cursor = Some(selected.clone());
        Some(selected)
    }

    pub(super) fn set_client_cursor_image(&mut self, image: CursorImageStatus) {
        if let CursorImageStatus::Surface(surface) = &image {
            self.refresh_cursor_surface(surface);
        }
        self.cursor_status = image;
        self.cursor_feedback_dirty = true;
    }

    fn refresh_cursor_surface(&mut self, surface: &WlSurface) {
        let (assignment, release_point, buffer_scale, buffer_transform, buffer_delta) =
            with_states(surface, |states| {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                let current = attributes.current();
                let mut syncobj = states.cached_state.get::<DrmSyncobjCachedState>();
                let syncobj = syncobj.current();
                syncobj.acquire_point = None;
                let release_point = syncobj.release_point.take();
                (
                    current.buffer.take(),
                    release_point,
                    current.buffer_scale,
                    current.buffer_transform,
                    current.buffer_delta.take(),
                )
            });
        signal_release_point(release_point, "cursor buffer consumed without GPU sampling");
        if let Some(buffer_delta) = buffer_delta {
            with_states(surface, |states| {
                let Some(attributes) = states.data_map.get::<CursorImageSurfaceData>() else {
                    return;
                };
                match attributes.lock() {
                    Ok(mut attributes) => attributes.hotspot -= buffer_delta,
                    Err(error) => warn!(
                        surface = ?surface.id(),
                        %error,
                        "cursor hotspot state is poisoned"
                    ),
                }
            });
        }

        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let cached = if get_dmabuf(&buffer).is_ok() {
                    buffer.release();
                    warn!(
                        surface = ?surface.id(),
                        "DMA-BUF cursor surfaces are not supported yet; using the compositor cursor"
                    );
                    CachedCursorSurface::Unsupported
                } else {
                    match copy_shm_buffer(&buffer) {
                        Ok(copied) => {
                            buffer.release();
                            let metadata = checked_buffer_scale(buffer_scale).map(|scale| {
                                SurfaceBufferMetadata {
                                    width: copied.width,
                                    height: copied.height,
                                    scale,
                                    transform: buffer_transform,
                                }
                            });
                            match metadata.and_then(|metadata| {
                                with_states(surface, |states| {
                                    surface_content_view(states, metadata)
                                })
                                .map(|view| (metadata, view))
                            }) {
                                Ok((metadata, view)) => {
                                    let mut pixels = copied.bgra_pixels;
                                    unpremultiply_alpha(&mut pixels);
                                    CachedCursorSurface::Image {
                                        pixels: pixels.into(),
                                        metadata,
                                        view,
                                    }
                                }
                                Err(error) => {
                                    warn!(
                                        surface = ?surface.id(),
                                        %error,
                                        "invalid cursor surface geometry; using the compositor cursor"
                                    );
                                    CachedCursorSurface::Unsupported
                                }
                            }
                        }
                        Err(error) => {
                            buffer.release();
                            warn!(
                                surface = ?surface.id(),
                                %error,
                                "unsupported cursor surface buffer; using the compositor cursor"
                            );
                            CachedCursorSurface::Unsupported
                        }
                    }
                };
                self.cursor_surfaces.surfaces.insert(surface.id(), cached);
            }
            Some(BufferAssignment::Removed) => {
                self.cursor_surfaces
                    .surfaces
                    .insert(surface.id(), CachedCursorSurface::Empty);
            }
            None => self.refresh_retained_cursor_view(surface, buffer_scale, buffer_transform),
        }

        if matches!(&self.cursor_status, CursorImageStatus::Surface(current) if current == surface)
        {
            self.cursor_feedback_dirty = true;
        }
    }

    fn refresh_retained_cursor_view(
        &mut self,
        surface: &WlSurface,
        buffer_scale: i32,
        buffer_transform: smithay::reexports::wayland_server::protocol::wl_output::Transform,
    ) {
        let Some(CachedCursorSurface::Image {
            pixels, metadata, ..
        }) = self.cursor_surfaces.surfaces.get(&surface.id())
        else {
            return;
        };
        let pixels = Arc::clone(pixels);
        let width = metadata.width;
        let height = metadata.height;
        let refreshed = checked_buffer_scale(buffer_scale).and_then(|scale| {
            let metadata = SurfaceBufferMetadata {
                width,
                height,
                scale,
                transform: buffer_transform,
            };
            with_states(surface, |states| surface_content_view(states, metadata))
                .map(|view| (metadata, view))
        });
        match refreshed {
            Ok((metadata, view)) => {
                self.cursor_surfaces.surfaces.insert(
                    surface.id(),
                    CachedCursorSurface::Image {
                        pixels,
                        metadata,
                        view,
                    },
                );
            }
            Err(error) => {
                warn!(surface = ?surface.id(), %error, "invalid retained cursor surface geometry");
                self.cursor_surfaces
                    .surfaces
                    .insert(surface.id(), CachedCursorSurface::Unsupported);
            }
        }
    }

    /// Must run after dispatch returns: Smithay can call cursor_image while
    /// holding its pointer mutex, so that callback cannot query current_focus.
    pub(crate) fn flush_cursor_feedback(&mut self) {
        if !self.cursor_feedback_dirty || self.shell_owns_cursor {
            return;
        }
        let Some(focus) = self
            .seat
            .get_pointer()
            .and_then(|pointer| pointer.current_focus())
        else {
            return;
        };
        let root = super::surface_tree::owning_root(&focus);
        let Some(owner) = self
            .toplevels
            .id_for_surface(&root)
            .or_else(|| self.popups.id_for_surface(&root))
        else {
            return;
        };
        self.cursor_feedback_dirty = false;
        let cursor = match &self.cursor_status {
            CursorImageStatus::Hidden => CursorImage::Hidden,
            CursorImageStatus::Named(icon) => CursorImage::Named(*icon),
            CursorImageStatus::Surface(surface) => {
                match self.cursor_surfaces.surfaces.get(&surface.id()) {
                    Some(CachedCursorSurface::Image {
                        pixels,
                        metadata,
                        view,
                    }) => {
                        let hotspot = cursor_hotspot(surface).unwrap_or_default();
                        match canonical_cursor(
                            pixels,
                            metadata.width,
                            metadata.height,
                            *view,
                            (hotspot.0 as f32, hotspot.1 as f32),
                        ) {
                            Ok(image) => CursorImage::Image(image),
                            Err(error) => {
                                warn!(%error, "invalid client cursor raster");
                                CursorImage::default()
                            }
                        }
                    }
                    Some(CachedCursorSurface::Unsupported) => {
                        CursorImage::Named(CursorIcon::Default)
                    }
                    Some(CachedCursorSurface::Empty) | None => CursorImage::Hidden,
                }
            }
        };
        self.pending_surface_events
            .publish_cursor(ClientCursorUpdate {
                surface: owner,
                cursor,
            });
    }
}

fn select_cursor(
    shell: CursorAppearance,
    explicit_override: bool,
    local_shell_owns: bool,
    client: Option<(weld_client::ClientSurfaceId, CursorImage)>,
) -> CursorImage {
    if !explicit_override
        && let Some((surface, image)) = client
        && !(surface.source() == crate::WAYLAND_CLIENT_SOURCE && local_shell_owns)
    {
        return image;
    }
    match shell {
        CursorAppearance::Hidden => CursorImage::Hidden,
        CursorAppearance::Named(icon) => CursorImage::Named(icon),
    }
}

fn cursor_hotspot(surface: &WlSurface) -> Option<(i32, i32)> {
    with_states(surface, |states| {
        states
            .data_map
            .get::<CursorImageSurfaceData>()
            .and_then(|attributes| attributes.lock().ok())
            .map(|attributes| (attributes.hotspot.x, attributes.hotspot.y))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_client::{ClientId, ClientSourceId, ClientSurfaceId};

    #[test]
    fn remote_feedback_respects_shell_override_without_borrowing_source_seat_focus() {
        let remote = ClientSurfaceId::new(ClientId::new(ClientSourceId::new(9), 1), 1);
        let local = ClientSurfaceId::new(ClientId::new(crate::WAYLAND_CLIENT_SOURCE, 1), 1);
        let shell = CursorAppearance::Named(CursorIcon::EwResize);
        let client = CursorImage::Named(CursorIcon::Text);
        assert_eq!(
            select_cursor(shell, false, true, Some((remote, client.clone()))),
            client
        );
        assert_eq!(
            select_cursor(shell, true, false, Some((remote, client.clone()))),
            CursorImage::Named(CursorIcon::EwResize)
        );
        assert_eq!(
            select_cursor(shell, false, true, Some((local, client.clone()))),
            CursorImage::Named(CursorIcon::EwResize)
        );
        assert_eq!(
            select_cursor(shell, false, false, Some((local, client))),
            CursorImage::Named(CursorIcon::Text)
        );
        assert_eq!(
            select_cursor(shell, false, false, None),
            CursorImage::Named(CursorIcon::EwResize)
        );
    }
}
