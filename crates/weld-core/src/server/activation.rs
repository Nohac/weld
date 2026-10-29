//! Bounded activation-token requests become non-stealing application attention hints.

use super::{PendingSurfaceEvent, PendingSurfaceEventKind, ServerState};
use crate::surface::SurfaceId;
use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::xdg_activation::{
        XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
    },
};
use weld_client::{ClientSurfaceRole, ToplevelState};

const TOKEN_LIFETIME: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_TOKENS: usize = 256;

impl XdgActivationHandler for ServerState {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation
    }

    fn token_created(&mut self, _: XdgActivationToken, _: XdgActivationTokenData) -> bool {
        self.activation
            .retain_tokens(|_, data| data.timestamp.elapsed() <= TOKEN_LIFETIME);
        self.activation.tokens().count() < MAX_TOKENS
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        self.activation.remove_token(&token);
        if data.timestamp.elapsed() > TOKEN_LIFETIME {
            return;
        }
        if let Some(id) = self.toplevels.id_for_surface(&surface)
            && !self
                .input_seats()
                .any(|seat| seat.focused_toplevel.get() == Some(id))
        {
            self.set_window_attention(id, true);
        }
    }
}

impl ServerState {
    pub(super) fn set_window_attention(&mut self, surface: SurfaceId, urgent: bool) {
        let Some(window) = self.toplevels.get_mut(surface) else {
            return;
        };
        if window.hints.urgent == urgent {
            return;
        }
        if let super::window::WindowSurface::X11 { window, .. } = &window.surface
            && let Err(error) = window.set_demands_attention(urgent)
        {
            tracing::warn!(%error, "could not publish X11 attention state");
        }
        window.hints.urgent = urgent;
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface,
            kind: PendingSurfaceEventKind::Role(ClientSurfaceRole::Toplevel(ToplevelState {
                parent: window.parent,
                decoration: window.decoration,
                hints: window.hints,
            })),
        });
        self.presentation_requested = true;
    }
}
