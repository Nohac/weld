//! Output-bound desktop roles, Smithay arrangement, and shared buffer publication.

use smithay::{
    desktop::{LayerSurface, layer_map_for_output},
    output::Output,
    reexports::wayland_server::{
        Resource,
        backend::ObjectId,
        protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    },
    wayland::{
        compositor::with_states,
        shell::{
            wlr_layer::{
                KeyboardInteractivity, Layer, LayerSurface as ProtocolLayerSurface,
                LayerSurfaceData, WlrLayerShellHandler, WlrLayerShellState,
            },
            xdg::PopupSurface,
        },
    },
};
use weld_client::{
    ClientOutputId, ClientSurfaceRole, DesktopLayer, LayerKeyboardInteractivity, LayerSurfaceState,
    LogicalPoint,
};

use super::{
    PendingSurfaceEvent, PendingSurfaceEventKind, ServerState,
    output::send_preferred_surface_scale,
    surface_tree::SurfaceTreeState,
    toplevel::{IndexedStore, SurfaceOutputAssignment, allocate_surface_id, client_id_for_surface},
};
use crate::{OutputId, geometry::LogicalRect, surface::SurfaceId};

pub(super) struct LayerState {
    pub surface: LayerSurface,
    pub tree: SurfaceTreeState,
    pub outputs: SurfaceOutputAssignment,
    published: Option<LayerSurfaceState>,
}

#[derive(Default)]
pub(super) struct LayerStore(pub IndexedStore<ObjectId, LayerState>);

impl LayerStore {
    pub fn id_for_surface(&self, surface: &WlSurface) -> Option<SurfaceId> {
        self.0.id_for_key(&surface.id())
    }
}

impl ServerState {
    pub(crate) fn take_output_work_areas(
        &mut self,
    ) -> impl Iterator<Item = (OutputId, LogicalRect)> + '_ {
        self.pending_work_areas.drain()
    }

    pub(super) fn arrange_layers(&mut self, output_id: OutputId) {
        let Some(output) = self.native_output(output_id) else {
            return;
        };
        let mut map = layer_map_for_output(&output);
        map.arrange();
        let zone = map.non_exclusive_zone();
        self.pending_work_areas.insert(
            output_id,
            LogicalRect::from_min_size(
                f64::from(zone.loc.x),
                f64::from(zone.loc.y),
                f64::from(zone.size.w),
                f64::from(zone.size.h),
            ),
        );
        let layouts = map
            .layers()
            .enumerate()
            .filter_map(|(index, layer)| {
                let id = self.layers.id_for_surface(layer.wl_surface())?;
                let geometry = map.layer_geometry(layer)?;
                let cached = layer.cached_state();
                let state = LayerSurfaceState {
                    output: ClientOutputId::new(output_id.raw()),
                    position: LogicalPoint::new(geometry.loc.x as f32, geometry.loc.y as f32),
                    layer: match cached.layer {
                        Layer::Background => DesktopLayer::Background,
                        Layer::Bottom => DesktopLayer::Bottom,
                        Layer::Top => DesktopLayer::Top,
                        Layer::Overlay => DesktopLayer::Overlay,
                    },
                    keyboard: match cached.keyboard_interactivity {
                        KeyboardInteractivity::None => LayerKeyboardInteractivity::None,
                        KeyboardInteractivity::Exclusive => LayerKeyboardInteractivity::Exclusive,
                        KeyboardInteractivity::OnDemand => LayerKeyboardInteractivity::OnDemand,
                    },
                    stack_index: u32::try_from(index).unwrap_or(u32::MAX),
                };
                Some((id, state))
            })
            .collect::<Vec<_>>();
        drop(map);
        for (id, state) in layouts {
            let Some(layer) = self.layers.0.get_mut(id) else {
                continue;
            };
            if layer.published == Some(state) {
                continue;
            }
            layer.published = Some(state);
            self.pending_surface_events.push_back(PendingSurfaceEvent {
                surface: id,
                kind: PendingSurfaceEventKind::Role(ClientSurfaceRole::Layer(state)),
            });
        }
        self.presentation_requested = true;
    }

    pub(super) fn commit_layer(&mut self, root: &WlSurface) -> bool {
        let Some(id) = self.layers.id_for_surface(root) else {
            return false;
        };
        let Some(layer) = self.layers.0.get(id) else {
            return true;
        };
        let output_id = layer.outputs.preferred;
        let native = layer.surface.clone();
        let configured = with_states(root, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .and_then(|state| state.lock().ok().map(|state| state.initial_configure_sent))
                .unwrap_or(false)
        });
        let was_mapped = layer.tree.client_mapped(root);
        let Some(layer) = self.layers.0.get_mut(id) else {
            return true;
        };
        let snapshot = layer.tree.update(id, root, &mut self.dmabuf_releases);
        let mapped = snapshot.client_mapped;
        if let Some(output) = self.native_output(output_id) {
            let mut map = layer_map_for_output(&output);
            if was_mapped && !mapped {
                map.unmap_layer(&native);
            } else if let Err(error) = map.map_layer(&native) {
                tracing::warn!(%error, "could not arrange layer surface");
                native.layer_surface().send_close();
            }
            send_preferred_surface_scale(&output, root);
        }
        self.arrange_layers(output_id);
        if !configured && !was_mapped {
            native.layer_surface().send_configure();
        }
        if was_mapped && !mapped {
            self.clear_input_focus_for_surface(root, self.event_time());
        }
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: id,
            kind: PendingSurfaceEventKind::TreeSnapshot(snapshot),
        });
        true
    }

    pub(super) fn remove_layer_subsurface(
        &mut self,
        root: &WlSurface,
        removed: &WlSurface,
    ) -> bool {
        let Some(id) = self.layers.id_for_surface(root) else {
            return false;
        };
        self.clear_input_focus_for_surface(removed, self.event_time());
        let Some(layer) = self.layers.0.get_mut(id) else {
            return true;
        };
        let snapshot = layer.tree.remove_surface(root, removed);
        self.presentation_requested = true;
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: id,
            kind: PendingSurfaceEventKind::TreeSnapshot(snapshot),
        });
        true
    }
}

impl WlrLayerShellHandler for ServerState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: ProtocolLayerSurface,
        output: Option<WlOutput>,
        _: Layer,
        namespace: String,
    ) {
        let output_id = match output {
            Some(output) => Output::from_resource(&output).and_then(|native| {
                self.outputs
                    .iter()
                    .find_map(|(id, output)| (output.native == native).then_some(*id))
            }),
            None => Some(self.primary_output),
        };
        let Some(output_id) = output_id else {
            surface.send_close();
            return;
        };
        let Some(client) = client_id_for_surface(surface.wl_surface()) else {
            surface.send_close();
            return;
        };
        let Some(id) = allocate_surface_id(&mut self.next_surface_id, client) else {
            surface.send_close();
            return;
        };
        let key = surface.wl_surface().id();
        let state = LayerState {
            surface: LayerSurface::new(surface.clone(), namespace),
            tree: SurfaceTreeState::default(),
            outputs: SurfaceOutputAssignment::primary(output_id),
            published: None,
        };
        if !self.layers.0.insert(id, key, state) {
            surface.send_close();
            return;
        }
        self.apply_surface_tree_outputs(
            surface.wl_surface(),
            &SurfaceOutputAssignment::primary(output_id),
            None,
        );
    }

    fn layer_destroyed(&mut self, surface: ProtocolLayerSurface) {
        let Some((id, layer)) = self.layers.0.remove_by_key(&surface.wl_surface().id()) else {
            return;
        };
        if let Some(output) = self.native_output(layer.outputs.preferred) {
            layer_map_for_output(&output).unmap_layer(&layer.surface);
        }
        self.forget_presentation(id);
        self.clear_input_focus_for_surface(surface.wl_surface(), self.event_time());
        self.pending_surface_events.push_back(PendingSurfaceEvent {
            surface: id,
            kind: PendingSurfaceEventKind::Destroyed,
        });
        self.arrange_layers(layer.outputs.preferred);
    }

    fn new_popup(&mut self, _: ProtocolLayerSurface, popup: PopupSurface) {
        self.publish_popup_layout(popup.wl_surface());
    }
}
