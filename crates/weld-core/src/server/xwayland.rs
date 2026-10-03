//! Rootless X11 lifecycle and window roles feeding the native surface pipeline.

use super::{
    ClientState, PendingSurfaceEvent, PendingSurfaceEventKind, ServerState,
    toplevel::{ToplevelState, allocate_surface_id},
    window::WindowSurface,
};
use crate::cursor::{
    CursorConfiguration, CursorIcon,
    theme::{fallback_image, load_theme_images},
};
use crate::surface::{SurfaceId, WindowDecoration, WindowResizeEdge};
use anyhow::{Context, Result, ensure};
use calloop::{
    LoopHandle,
    timer::{TimeoutAction, Timer},
};
use smithay::{
    reexports::wayland_server::{
        Client, DisplayHandle, backend::DisconnectReason, protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Rectangle},
    wayland::{
        compositor::{BufferAssignment, CompositorClientState, SurfaceAttributes, with_states},
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandClientData, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge, WmWindowProperty, XwmId},
    },
};
use std::{collections::HashMap, process::Stdio, sync::Arc, time::Duration};
use tracing::{info, warn};
use weld_client::{
    ClientId, ClientSurfaceMetadata, ClientSurfaceRole, LogicalPoint, PopupState,
    ToplevelState as ClientToplevelState,
};

pub(super) struct XwaylandState {
    pub shell: XWaylandShellState,
    pub wm: Option<X11Wm>,
    pub display: Option<u32>,
    cursor_configuration: CursorConfiguration,
    windows: HashMap<u32, X11Window>,
}

struct X11Window {
    native: Arc<X11Surface>,
    client: ClientId,
    pid: Option<u32>,
    surface: Option<(SurfaceId, WlSurface)>,
    published_role: Option<ClientSurfaceRole>,
    published_metadata: Option<ClientSurfaceMetadata>,
}

/// Remains on the wl_surface after retirement so late commits release buffers.
struct AssociatedX11Surface;

pub(super) fn awaiting_x11_association(surface: &WlSurface) -> bool {
    with_states(surface, |states| {
        states.data_map.get::<AssociatedX11Surface>().is_none()
    })
}

impl XwaylandState {
    pub(super) fn new(display: &DisplayHandle) -> Self {
        Self {
            shell: XWaylandShellState::new::<ServerState>(display),
            wm: None,
            display: None,
            cursor_configuration: CursorConfiguration::default(),
            windows: HashMap::new(),
        }
    }
}

pub(super) fn compositor_state(client: &Client) -> &CompositorClientState {
    if let Some(data) = client.get_data::<XWaylandClientData>() {
        return &data.compositor_state;
    }
    &client
        .get_data::<ClientState>()
        .expect("native clients are registered with compositor state")
        .compositor_state
}

impl ServerState {
    pub(crate) fn set_x11_cursor_configuration(&mut self, configuration: CursorConfiguration) {
        if self.xwayland.cursor_configuration != configuration {
            self.xwayland.cursor_configuration = configuration;
            self.update_x11_default_cursor();
        }
    }

    fn update_x11_default_cursor(&mut self) {
        let Some(wm) = &mut self.xwayland.wm else {
            return;
        };
        let configuration = &self.xwayland.cursor_configuration;
        let images =
            load_theme_images(configuration.theme(), CursorIcon::Default).unwrap_or_else(|error| {
                warn!(%error, "using built-in X11 default cursor");
                Arc::from([fallback_image()])
            });
        let Some(image) = images
            .iter()
            .min_by_key(|image| image.size.abs_diff(configuration.size()))
        else {
            return;
        };
        let result = (|| -> Result<()> {
            ensure!(
                image.width > 0 && image.height > 0 && image.width <= 512 && image.height <= 512,
                "invalid X11 default cursor extent"
            );
            ensure!(
                image.pixels_rgba.len() >= image.width as usize * image.height as usize * 4,
                "short X11 default cursor raster"
            );
            wm.set_cursor(
                &image.pixels_rgba,
                (u16::try_from(image.width)?, u16::try_from(image.height)?).into(),
                (
                    u16::try_from(image.xhot.min(image.width - 1))?,
                    u16::try_from(image.yhot.min(image.height - 1))?,
                )
                    .into(),
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            warn!(%error, "could not set themed X11 default cursor");
        }
    }

    /// XWayland adds surface-local input to its own root coordinates before
    /// hit-testing. Keep the presenter's selected window above X11 siblings.
    pub(super) fn raise_x11_surface(&mut self, surface: &WlSurface) {
        let root = super::surface_tree::owning_root(surface);
        let Some(window) = self
            .toplevels
            .id_for_surface(&root)
            .and_then(|id| self.toplevels.get(id))
        else {
            return;
        };
        if let WindowSurface::X11 { window, .. } = &window.surface
            && let Some(wm) = &mut self.xwayland.wm
            && let Err(error) = wm.raise_window(window.as_ref())
        {
            warn!(%error, "could not raise X11 input target");
        }
    }

    pub(crate) fn start_xwayland(&mut self, handle: LoopHandle<'static, Self>) -> Result<()> {
        let (xwayland, client) = XWayland::spawn(
            &self.display_handle,
            None,
            std::iter::empty::<(String, String)>(),
            ["-nolisten", "tcp"],
            true,
            Stdio::null(),
            Stdio::inherit(),
            |_| {},
        )
        .context("could not start XWayland; install Xwayland or omit --xwayland")?;
        self.xwayland.display = Some(xwayland.display_number());
        let wm_handle = handle.clone();
        let startup_client = client.clone();
        handle.insert_source(xwayland, move |event, _, state| match event {
            XWaylandEvent::Ready { x11_socket, display_number } => {
                let manager = X11Wm::start_wm(wm_handle.clone(), &state.display_handle, x11_socket, client.clone());
                match manager {
                    Ok(wm) => {
                        state.xwayland.wm = Some(wm);
                        state.update_x11_default_cursor();
                        info!(display = %format_args!(":{display_number}"), "rootless XWayland ready");
                    }
                    Err(error) => {
                        warn!(%error, "could not start X11 window manager");
                        state.fail_xwayland(&client);
                    }
                }
            }
            XWaylandEvent::Error => {
                warn!("XWayland startup failed");
                state.fail_xwayland(&client);
            }
        }).map_err(|error| anyhow::anyhow!("could not register XWayland: {error}"))?;
        handle
            .insert_source(
                Timer::from_duration(Duration::from_secs(10)),
                move |_, _, state| {
                    if state.xwayland.display.is_some() && state.xwayland.wm.is_none() {
                        warn!("XWayland did not become ready within ten seconds");
                        state.fail_xwayland(&startup_client);
                    }
                    TimeoutAction::Drop
                },
            )
            .map_err(|error| {
                anyhow::anyhow!("could not register XWayland startup deadline: {error}")
            })?;
        Ok(())
    }

    fn fail_xwayland(&mut self, client: &Client) {
        self.xwayland.display = None;
        self.display_handle
            .backend_handle()
            .kill_client(client.id(), DisconnectReason::ConnectionClosed);
    }

    pub(crate) fn x11_display(&self) -> Option<u32> {
        self.xwayland.display
    }

    fn track_x11_window(&mut self, window: X11Surface) {
        // XRes obtains the server's client PID, rather than trusting _NET_WM_PID.
        let pid = window.get_client_pid().ok().filter(|pid| *pid != 0);
        let shared = pid.and_then(|pid| {
            self.xwayland
                .windows
                .values()
                .find(|other| other.pid == Some(pid))
                .map(|other| other.client)
        });
        let Some(client) = shared.or_else(|| self.allocate_client_id()) else {
            warn!("X11 client identity space exhausted");
            return;
        };
        self.xwayland.windows.insert(
            window.window_id(),
            X11Window {
                native: Arc::new(window),
                client,
                pid,
                surface: None,
                published_role: None,
                published_metadata: None,
            },
        );
    }

    fn x11_owner(&self, window: &X11Window) -> Option<&X11Window> {
        let known_owner = match window.published_role {
            Some(ClientSurfaceRole::Popup(popup)) => Some(popup.owner),
            _ => self
                .seat
                .get_pointer()
                .and_then(|pointer| pointer.current_focus())
                .and_then(|surface| {
                    self.toplevels
                        .id_for_surface(&super::surface_tree::owning_root(&surface))
                })
                .filter(|id| id.client() == window.client)
                .and_then(|id| {
                    let pointed = self.xwayland.windows.values().find(|window| {
                        window
                            .surface
                            .as_ref()
                            .is_some_and(|(surface, _)| *surface == id)
                    })?;
                    match pointed.published_role {
                        Some(ClientSurfaceRole::Popup(popup)) => Some(popup.owner),
                        Some(ClientSurfaceRole::Toplevel(_)) => Some(id),
                        _ => None,
                    }
                })
                .or(self.focused_toplevel),
        };
        let mut parent_id = window.native.is_transient_for().or_else(|| {
            window
                .native
                .is_override_redirect()
                .then(|| {
                    self.xwayland
                        .windows
                        .values()
                        .find(|other| {
                            other.client == window.client
                                && !other.native.is_override_redirect()
                                && other
                                    .surface
                                    .as_ref()
                                    .is_some_and(|(id, _)| Some(*id) == known_owner)
                        })
                        .map(|other| other.native.window_id())
                })
                .flatten()
        })?;
        // Nested override-redirect menus share the toplevel's coordinate space.
        for _ in 0..self.xwayland.windows.len() {
            if parent_id == window.native.window_id() {
                return None;
            }
            let parent = self.xwayland.windows.get(&parent_id)?;
            if !parent.native.is_override_redirect() {
                return Some(parent);
            }
            parent_id = parent.native.is_transient_for()?;
        }
        None
    }

    fn publish_x11_role(&mut self, window_id: u32) {
        let Some(window) = self.xwayland.windows.get(&window_id) else {
            return;
        };
        let Some((id, _)) = &window.surface else {
            return;
        };
        let id = *id;
        let parent = self.x11_owner(window);
        let parent_id = parent.and_then(|parent| parent.surface.as_ref().map(|(id, _)| *id));
        let role = if window.native.is_override_redirect() {
            let Some((owner, parent)) = parent_id.zip(parent) else {
                return;
            };
            let content_origin = if parent.native.is_decorated() {
                parent.native.geometry().loc
            } else {
                (0, 0).into()
            };
            let position = window.native.last_configure().loc
                - parent.native.last_configure().loc
                - content_origin;
            ClientSurfaceRole::Popup(PopupState {
                owner,
                position: LogicalPoint::new(position.x as f32, position.y as f32),
                stack_index: i32::try_from(id.local()).unwrap_or(i32::MAX),
            })
        } else {
            ClientSurfaceRole::Toplevel(ClientToplevelState {
                parent: parent_id,
                decoration: if window.native.is_decorated() {
                    WindowDecoration::ClientSide
                } else {
                    WindowDecoration::ServerSide
                },
            })
        };
        let metadata =
            ClientSurfaceMetadata::truncated(window.native.class(), window.native.title());
        let Some(window) = self.xwayland.windows.get_mut(&window_id) else {
            return;
        };
        if window.published_role.as_ref() != Some(&role) {
            tracing::debug!(window = window_id, ?role, "published X11 surface role");
            window.published_role = Some(role);
            self.pending_surface_events.push_back(PendingSurfaceEvent {
                surface: id,
                kind: PendingSurfaceEventKind::Role(role),
            });
        }
        if let Some(toplevel) = self.toplevels.get_mut(id) {
            toplevel.parent = parent_id;
            if let ClientSurfaceRole::Toplevel(state) = role {
                toplevel.decoration = state.decoration;
            }
        }
        if window.published_metadata.as_ref() != Some(&metadata) {
            window.published_metadata = Some(metadata.clone());
            self.pending_surface_events.push_back(PendingSurfaceEvent {
                surface: id,
                kind: PendingSurfaceEventKind::Metadata(metadata),
            });
        }
    }

    fn unmap_x11_window(&mut self, window_id: u32) {
        if let Some((owner, _)) = self
            .xwayland
            .windows
            .get(&window_id)
            .and_then(|window| window.surface.as_ref())
        {
            let dependents: Vec<_> = self.xwayland.windows.iter().filter_map(|(id, window)| {
                matches!(window.published_role, Some(ClientSurfaceRole::Popup(popup)) if popup.owner == *owner).then_some(*id)
            }).collect();
            for dependent in dependents {
                self.unmap_x11_window(dependent);
            }
        }
        if let Some(window) = self.xwayland.windows.get_mut(&window_id) {
            window.published_role = None;
            window.published_metadata = None;
            if let Some((_, surface)) = window.surface.take() {
                self.retire_window(&surface);
            }
        }
    }
}

impl XWaylandShellHandler for ServerState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland.shell
    }
    fn surface_associated(&mut self, _: XwmId, surface: WlSurface, native: X11Surface) {
        with_states(&surface, |states| {
            states.data_map.insert_if_missing(|| AssociatedX11Surface);
        });
        let key = native.window_id();
        let Some(window) = self.xwayland.windows.get(&key) else {
            return;
        };
        if window
            .surface
            .as_ref()
            .is_some_and(|(_, old)| old == &surface)
        {
            return;
        }
        let client = window.client;
        let native = Arc::clone(&window.native);
        self.unmap_x11_window(key);
        let Some(id) = allocate_surface_id(&mut self.next_surface_id, client) else {
            return;
        };
        let state = ToplevelState::x11(native, surface.clone(), self.primary_output);
        if !self.toplevels.insert(id, state) {
            return;
        }
        if let Some(window) = self.xwayland.windows.get_mut(&key) {
            window.surface = Some((id, surface.clone()));
        }
        self.enter_primary_output(&surface);
        let windows: Vec<_> = self.xwayland.windows.keys().copied().collect();
        for window in windows {
            self.publish_x11_role(window);
        }
        info!(
            window = key,
            surface_id = id.local(),
            "associated X11 window"
        );
        // The X11 notification can follow the first commit, or association can
        // run inside its pre-commit hook before the pending buffer is current.
        let committed_buffer = with_states(&surface, |states| {
            matches!(
                states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .buffer,
                Some(BufferAssignment::NewBuffer(_))
            )
        });
        if committed_buffer {
            self.update_surface_tree(id, &surface);
        }
        self.presentation_requested = true;
    }
}

impl XwmHandler for ServerState {
    fn xwm_state(&mut self, _: XwmId) -> &mut X11Wm {
        self.xwayland
            .wm
            .as_mut()
            .expect("XWM callbacks run only after start_wm registered the manager")
    }
    fn new_window(&mut self, _: XwmId, window: X11Surface) {
        self.track_x11_window(window);
    }
    fn new_override_redirect_window(&mut self, _: XwmId, window: X11Surface) {
        self.track_x11_window(window);
    }
    fn map_window_request(&mut self, _: XwmId, window: X11Surface) {
        if let Err(error) = window.set_mapped(true) {
            warn!(%error, "could not map X11 window");
            return;
        }
        if let Err(error) = window.configure(None) {
            warn!(%error, "could not configure mapped X11 window");
        }
    }
    fn mapped_override_redirect_window(&mut self, _: XwmId, window: X11Surface) {
        self.publish_x11_role(window.window_id());
    }
    fn unmapped_window(&mut self, _: XwmId, window: X11Surface) {
        self.unmap_x11_window(window.window_id());
    }
    fn destroyed_window(&mut self, _: XwmId, window: X11Surface) {
        self.unmap_x11_window(window.window_id());
        self.xwayland.windows.remove(&window.window_id());
    }
    fn configure_request(
        &mut self,
        _: XwmId,
        window: X11Surface,
        _: Option<i32>,
        _: Option<i32>,
        width: Option<u32>,
        height: Option<u32>,
        _: Option<Reorder>,
    ) {
        let mut geometry = window.last_configure();
        let admitted = self
            .xwayland
            .windows
            .get(&window.window_id())
            .is_some_and(|window| window.surface.is_some());
        if !admitted {
            if let Some(width) = width {
                geometry.size.w = i32::try_from(width.max(1)).unwrap_or(i32::MAX);
            }
            if let Some(height) = height {
                geometry.size.h = i32::try_from(height.max(1)).unwrap_or(i32::MAX);
            }
        }
        if let Err(error) = window.configure(geometry) {
            warn!(%error, "could not answer X11 configure request");
        }
    }
    fn configure_notify(
        &mut self,
        _: XwmId,
        window: X11Surface,
        _: Rectangle<i32, Logical>,
        _: Option<u32>,
    ) {
        self.publish_x11_role(window.window_id());
    }
    fn property_notify(&mut self, _: XwmId, window: X11Surface, _: WmWindowProperty) {
        self.publish_x11_role(window.window_id());
    }
    fn resize_request(&mut self, _: XwmId, window: X11Surface, button: u32, edge: ResizeEdge) {
        let edge = match edge {
            ResizeEdge::Top => WindowResizeEdge::Top,
            ResizeEdge::Bottom => WindowResizeEdge::Bottom,
            ResizeEdge::Left => WindowResizeEdge::Left,
            ResizeEdge::Right => WindowResizeEdge::Right,
            ResizeEdge::TopLeft => WindowResizeEdge::TopLeft,
            ResizeEdge::TopRight => WindowResizeEdge::TopRight,
            ResizeEdge::BottomLeft => WindowResizeEdge::BottomLeft,
            ResizeEdge::BottomRight => WindowResizeEdge::BottomRight,
        };
        if let Some((id, _)) = self
            .xwayland
            .windows
            .get(&window.window_id())
            .and_then(|window| window.surface.as_ref())
        {
            self.begin_x11_interaction(*id, button, Some(edge));
        }
    }
    fn move_request(&mut self, _: XwmId, window: X11Surface, button: u32) {
        if let Some((id, _)) = self
            .xwayland
            .windows
            .get(&window.window_id())
            .and_then(|window| window.surface.as_ref())
        {
            self.begin_x11_interaction(*id, button, None);
        }
    }
    fn disconnected(&mut self, _: XwmId) {
        self.xwayland.display = None;
        let surfaces: Vec<_> = self
            .xwayland
            .windows
            .drain()
            .filter_map(|(_, window)| window.surface.map(|(_, surface)| surface))
            .collect();
        for surface in surfaces {
            self.retire_window(&surface);
        }
        warn!("XWayland disconnected; native Wayland session remains available");
    }
}
