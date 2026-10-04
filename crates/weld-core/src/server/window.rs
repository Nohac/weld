//! Native window controls over the shared Wayland surface/buffer transport.

use crate::surface::Extent;
use smithay::xwayland::xwm::WmWindowType;
use smithay::{
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{Logical, Rectangle, Size},
    wayland::{
        compositor::with_states,
        shell::xdg::{SurfaceCachedState, ToplevelSurface},
    },
    xwayland::X11Surface,
};
use std::sync::Arc;
use tracing::warn;
use weld_client::{ToplevelHints, ToplevelKind, ToplevelLayout};

#[derive(Clone)]
pub(super) enum WindowSurface {
    Xdg(ToplevelSurface),
    X11 {
        window: Arc<X11Surface>,
        surface: WlSurface,
    },
}

impl WindowSurface {
    pub(super) fn hints(&self) -> ToplevelHints {
        let (kind, minimum, maximum) = match self {
            Self::Xdg(window) => with_states(window.wl_surface(), |states| {
                let mut cached = states.cached_state.get::<SurfaceCachedState>();
                let current = cached.current();
                (ToplevelKind::Normal, current.min_size, current.max_size)
            }),
            Self::X11 { window, .. } => {
                let kind = match window.window_type() {
                    Some(WmWindowType::Dialog) => ToplevelKind::Dialog,
                    Some(WmWindowType::Utility) => ToplevelKind::Utility,
                    Some(WmWindowType::Toolbar) => ToplevelKind::Toolbar,
                    Some(WmWindowType::Splash) => ToplevelKind::Splash,
                    _ if window.is_modal() => ToplevelKind::Dialog,
                    _ => ToplevelKind::Normal,
                };
                (
                    kind,
                    window.min_size().unwrap_or_default(),
                    window.max_size().unwrap_or_default(),
                )
            }
        };
        ToplevelHints {
            kind,
            min_size: Extent::new(minimum.w.max(0) as u32, minimum.h.max(0) as u32),
            max_size: Extent::new(maximum.w.max(0) as u32, maximum.h.max(0) as u32),
        }
    }

    pub(super) fn set_layout(&self, layout: ToplevelLayout) {
        let tiled = layout == ToplevelLayout::Tiled;
        match self {
            Self::Xdg(window) => window.with_pending_state(|state| {
                for edge in [
                    xdg_toplevel::State::TiledLeft,
                    xdg_toplevel::State::TiledRight,
                    xdg_toplevel::State::TiledTop,
                    xdg_toplevel::State::TiledBottom,
                ] {
                    if tiled {
                        state.states.set(edge);
                    } else {
                        state.states.unset(edge);
                    }
                }
            }),
            Self::X11 { window, .. } if !window.is_override_redirect() => {
                if let Err(error) = window.set_maximized(tiled) {
                    warn!(%error, "could not publish X11 tiled state");
                }
            }
            Self::X11 { .. } => {}
        }
    }
    pub(super) fn geometry(&self) -> Option<Rectangle<i32, Logical>> {
        match self {
            Self::Xdg(window) => xdg_geometry(window.wl_surface()),
            Self::X11 { window, .. } => window.is_decorated().then(|| window.geometry()),
        }
    }
    pub(super) fn wl_surface(&self) -> &WlSurface {
        match self {
            Self::Xdg(window) => window.wl_surface(),
            Self::X11 { surface, .. } => surface,
        }
    }
    pub(super) fn alive(&self) -> bool {
        match self {
            Self::Xdg(window) => window.alive(),
            Self::X11 { window, surface } => window.alive() && surface.is_alive(),
        }
    }
    pub(super) fn close(&self) {
        match self {
            Self::Xdg(window) => window.send_close(),
            Self::X11 { window, .. } => {
                if let Err(error) = window.close() {
                    warn!(%error, "could not close X11 window");
                }
            }
        }
    }
    pub(super) fn set_activated(&self, activated: bool) {
        match self {
            Self::Xdg(window) => {
                if !window.is_initial_configure_sent() {
                    return;
                }
                window.with_pending_state(|state| {
                    if activated {
                        state.states.set(xdg_toplevel::State::Activated);
                    } else {
                        state.states.unset(xdg_toplevel::State::Activated);
                    }
                });
                window.send_pending_configure();
            }
            Self::X11 { window, .. } => {
                if !window.is_override_redirect()
                    && let Err(error) = window.set_activated(activated)
                {
                    warn!(%error, "could not activate X11 window");
                }
            }
        }
    }
    pub(super) fn set_resizing(&self, resizing: bool) -> bool {
        let Self::Xdg(window) = self else {
            return false;
        };
        window.with_pending_state(|state| {
            if resizing {
                state.states.set(xdg_toplevel::State::Resizing)
            } else {
                state.states.unset(xdg_toplevel::State::Resizing)
            }
        })
    }
    pub(super) fn set_fullscreen(&self, fullscreen: bool) {
        match self {
            Self::Xdg(window) => {
                window.with_pending_state(|state| {
                    if fullscreen {
                        state.states.set(xdg_toplevel::State::Fullscreen);
                    } else {
                        state.states.unset(xdg_toplevel::State::Fullscreen);
                    }
                });
            }
            Self::X11 { window, .. } => {
                if !window.is_override_redirect()
                    && let Err(error) = window.set_fullscreen(fullscreen)
                {
                    warn!(%error, "could not publish X11 fullscreen state");
                }
            }
        }
    }
    pub(super) fn flush_configure(&self) {
        if let Self::Xdg(window) = self
            && window.is_initial_configure_sent()
        {
            window.send_pending_configure();
        }
    }
    pub(super) fn stage_size(
        &self,
        requested: Extent,
        fullscreen: bool,
        layout: ToplevelLayout,
    ) -> bool {
        if !self.alive()
            || matches!(self, Self::X11 { window, .. } if window.is_override_redirect())
        {
            return false;
        }
        let requested = if fullscreen || layout == ToplevelLayout::Tiled {
            requested
        } else {
            self.hints().constrain(requested)
        };
        let size = Size::<i32, Logical>::from((
            i32::try_from(requested.width.max(1)).unwrap_or(i32::MAX),
            i32::try_from(requested.height.max(1)).unwrap_or(i32::MAX),
        ));
        match self {
            Self::Xdg(window) => window.with_pending_state(|state| {
                if state.size == Some(size) {
                    false
                } else {
                    state.size = Some(size);
                    true
                }
            }),
            Self::X11 { window, .. } => {
                let mut geometry = window.last_configure();
                if geometry.size == size {
                    return false;
                }
                geometry.size = size;
                if let Err(error) = window.configure(geometry) {
                    warn!(%error, "could not configure X11 window");
                    return false;
                }
                true
            }
        }
    }
}

pub(super) fn xdg_geometry(surface: &WlSurface) -> Option<Rectangle<i32, Logical>> {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
    })
}
