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

use crate::cursor::{ClientCursorImage, CursorAppearance, CursorImage, unpremultiply_bgra};

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
        if self.shell_owns_cursor {
            self.apply_shell_cursor();
        }
    }

    pub(super) fn set_shell_cursor_ownership(&mut self, owned: bool) {
        if self.shell_owns_cursor == owned {
            return;
        }
        self.shell_owns_cursor = owned;
        if owned {
            self.apply_shell_cursor();
        } else {
            let default = CursorImageStatus::default_named();
            if self.cursor_status != default {
                self.cursor_status = default;
                self.queue_current_cursor();
            }
        }
    }

    fn apply_shell_cursor(&mut self) {
        let status = match self.shell_cursor {
            CursorAppearance::Hidden => CursorImageStatus::Hidden,
            CursorAppearance::Named(icon) => CursorImageStatus::Named(icon),
        };
        if self.cursor_status != status {
            self.cursor_status = status;
            self.queue_current_cursor();
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
            self.pending_cursor_image = Some(CursorImage::Named(CursorIcon::Default));
        }
    }

    pub(crate) fn take_cursor_image(&mut self) -> Option<CursorImage> {
        self.pending_cursor_image.take()
    }

    pub(super) fn set_client_cursor_image(&mut self, image: CursorImageStatus) {
        if let CursorImageStatus::Surface(surface) = &image {
            self.refresh_cursor_surface(surface);
        }
        self.cursor_status = image;
        self.queue_current_cursor();
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
                                    unpremultiply_bgra(&mut pixels);
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
            self.queue_current_cursor();
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

    fn queue_current_cursor(&mut self) {
        self.pending_cursor_image = Some(match &self.cursor_status {
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
                        CursorImage::Surface(ClientCursorImage {
                            pixels: Arc::clone(pixels),
                            width: metadata.width,
                            height: metadata.height,
                            view: *view,
                            hotspot_x: hotspot.0 as f32,
                            hotspot_y: hotspot.1 as f32,
                        })
                    }
                    Some(CachedCursorSurface::Unsupported) => {
                        CursorImage::Named(CursorIcon::Default)
                    }
                    Some(CachedCursorSurface::Empty) | None => CursorImage::Hidden,
                }
            }
        });
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
