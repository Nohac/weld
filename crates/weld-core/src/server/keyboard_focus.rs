//! Seat keyboard targets preserve X11 input-model and Wayland delivery semantics.

use super::{ServerState, window::WindowSurface};
use smithay::{
    backend::input::{InputTime, KeyEvent},
    desktop::PopupKind,
    input::{
        Seat,
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
    },
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::{IsAlive, Serial},
    wayland::seat::WaylandFocus,
    xwayland::X11Surface,
};
use std::{borrow::Cow, sync::Arc};

#[derive(Clone, Debug, PartialEq)]
pub enum KeyboardFocus {
    Wayland(WlSurface),
    X11 {
        window: Arc<X11Surface>,
        surface: WlSurface,
    },
}

impl From<WlSurface> for KeyboardFocus {
    fn from(surface: WlSurface) -> Self {
        Self::Wayland(surface)
    }
}
impl From<PopupKind> for KeyboardFocus {
    fn from(popup: PopupKind) -> Self {
        Self::Wayland(popup.wl_surface().clone())
    }
}
impl From<&WindowSurface> for KeyboardFocus {
    fn from(window: &WindowSurface) -> Self {
        match window {
            WindowSurface::Xdg(window) => Self::Wayland(window.wl_surface().clone()),
            WindowSurface::X11 { window, surface } => Self::X11 {
                window: window.clone(),
                surface: surface.clone(),
            },
        }
    }
}
impl IsAlive for KeyboardFocus {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(surface) => surface.is_alive(),
            Self::X11 { window, surface } => window.alive() && surface.is_alive(),
        }
    }
}
impl WaylandFocus for KeyboardFocus {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(surface) | Self::X11 { surface, .. } => Some(Cow::Borrowed(surface)),
        }
    }
}
impl KeyboardFocus {
    fn target(&self) -> &dyn KeyboardTarget<ServerState> {
        match self {
            Self::Wayland(surface) => surface,
            Self::X11 { window, .. } => window.as_ref(),
        }
    }
}

impl From<KeyboardFocus> for WlSurface {
    fn from(focus: KeyboardFocus) -> Self {
        match focus {
            KeyboardFocus::Wayland(surface) | KeyboardFocus::X11 { surface, .. } => surface,
        }
    }
}
impl KeyboardTarget<ServerState> for KeyboardFocus {
    fn enter(
        &self,
        seat: &Seat<ServerState>,
        state: &mut ServerState,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        self.target().enter(seat, state, keys, serial);
    }
    fn leave(&self, seat: &Seat<ServerState>, state: &mut ServerState, serial: Serial) {
        self.target().leave(seat, state, serial);
    }
    fn key(
        &self,
        seat: &Seat<ServerState>,
        state: &mut ServerState,
        key: KeysymHandle<'_>,
        event: KeyEvent,
        serial: Serial,
        time: InputTime,
    ) {
        self.target().key(seat, state, key, event, serial, time);
    }
    fn modifiers(
        &self,
        seat: &Seat<ServerState>,
        state: &mut ServerState,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        self.target().modifiers(seat, state, modifiers, serial);
    }
}
