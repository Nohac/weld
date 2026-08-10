//! Lock-safe import and protocol-neutral snapshots of one Wayland surface tree.

use std::collections::{HashMap, HashSet};

use smithay::{
    reexports::wayland_server::{
        Resource,
        backend::ObjectId,
        protocol::{wl_buffer::WlBuffer, wl_output, wl_surface::WlSurface},
    },
    utils::{Logical, Point, Rectangle},
    wayland::{
        compositor::{
            BufferAssignment, SUBSURFACE_ROLE, SubsurfaceCachedState, SurfaceAttributes,
            TraversalAction, get_parent, is_sync_subsurface, with_states, with_surface_tree_upward,
        },
        shell::xdg::SurfaceCachedState as XdgSurfaceCachedState,
    },
};
use tracing::warn;

use crate::surface::{
    SurfaceBufferUpdate, SurfaceContentView, SurfaceId, SurfaceLayerId, SurfaceLayerPlacement,
    SurfaceTreeSnapshot,
};

use super::shm::{
    SurfaceBufferMetadata, checked_buffer_scale, copy_shm_buffer, surface_content_view,
};

pub(super) struct SurfaceTreeState {
    buffers: HashMap<ObjectId, CachedSurfaceBuffer>,
    nodes: Vec<TreeNode>,
    root_geometry: Option<Rectangle<i32, Logical>>,
    next_layer_id: Option<u64>,
}

impl Default for SurfaceTreeState {
    fn default() -> Self {
        Self {
            buffers: HashMap::new(),
            nodes: Vec::new(),
            root_geometry: None,
            next_layer_id: Some(1),
        }
    }
}

struct CachedSurfaceBuffer {
    layer: SurfaceLayerId,
    metadata: Option<SurfaceBufferMetadata>,
    view: Option<SurfaceContentView>,
    opaque: bool,
    client_mapped: bool,
}

#[derive(Clone)]
struct TreeNode {
    surface: WlSurface,
    object_id: ObjectId,
    parent: Option<ObjectId>,
    position: Point<i32, Logical>,
}

struct CommittedNode {
    node: TreeNode,
    assignment: Option<BufferAssignment>,
    buffer_scale: i32,
    buffer_transform: wl_output::Transform,
}

#[derive(Clone)]
struct TraversalContext {
    position: Point<i32, Logical>,
    parent: Option<ObjectId>,
}

impl Default for TraversalContext {
    fn default() -> Self {
        Self {
            position: (0, 0).into(),
            parent: None,
        }
    }
}

impl SurfaceTreeState {
    pub(super) fn should_process_commit(surface: &WlSurface) -> bool {
        !is_sync_subsurface(surface)
    }

    pub(super) fn update(
        &mut self,
        surface_id: SurfaceId,
        root: &WlSurface,
    ) -> SurfaceTreeSnapshot {
        let mut committed = Vec::new();
        let mut root_geometry = None;
        with_surface_tree_upward(
            root,
            TraversalContext::default(),
            |surface, states, context| {
                let position = context.position + surface_offset(states);
                TraversalAction::DoChildren(TraversalContext {
                    position,
                    parent: Some(surface.id()),
                })
            },
            |surface, states, context| {
                let position = context.position + surface_offset(states);
                if context.parent.is_none() {
                    let mut xdg_state = states.cached_state.get::<XdgSurfaceCachedState>();
                    root_geometry = xdg_state.current().geometry;
                }
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                let current = attributes.current();
                committed.push(CommittedNode {
                    node: TreeNode {
                        surface: surface.clone(),
                        object_id: surface.id(),
                        parent: context.parent.clone(),
                        position,
                    },
                    assignment: current.buffer.take(),
                    buffer_scale: current.buffer_scale,
                    buffer_transform: current.buffer_transform,
                });
            },
            |_, _, _| true,
        );
        self.root_geometry = root_geometry;

        let live_ids = committed
            .iter()
            .map(|committed| committed.node.object_id.clone())
            .collect::<HashSet<_>>();
        self.buffers
            .retain(|object_id, _| live_ids.contains(object_id));
        self.nodes = committed
            .iter()
            .map(|committed| committed.node.clone())
            .collect();

        let mut pixel_updates = HashMap::new();
        for committed in committed {
            self.apply_commit(surface_id, committed, &mut pixel_updates);
        }
        self.snapshot(root.id(), pixel_updates)
    }

    pub(super) fn remove_surface(
        &mut self,
        root: &WlSurface,
        removed: &WlSurface,
    ) -> SurfaceTreeSnapshot {
        let removed_id = removed.id();
        let mut removed_ids = HashSet::from([removed_id.clone()]);
        loop {
            let descendants = self
                .nodes
                .iter()
                .filter(|node| {
                    node.parent
                        .as_ref()
                        .is_some_and(|parent| removed_ids.contains(parent))
                })
                .map(|node| node.object_id.clone())
                .filter(|object_id| !removed_ids.contains(object_id))
                .collect::<Vec<_>>();
            if descendants.is_empty() {
                break;
            }
            removed_ids.extend(descendants);
        }
        self.nodes
            .retain(|node| !removed_ids.contains(&node.object_id));
        self.buffers
            .retain(|object_id, _| !removed_ids.contains(object_id));
        self.snapshot(root.id(), HashMap::new())
    }

    pub(super) fn client_mapped(&self, root: &WlSurface) -> bool {
        self.buffers
            .get(&root.id())
            .is_some_and(|buffer| buffer.client_mapped)
    }

    pub(super) fn displayable(&self, root: &WlSurface) -> bool {
        self.buffers
            .get(&root.id())
            .is_some_and(|buffer| buffer.metadata.is_some() && buffer.view.is_some())
    }

    fn apply_commit(
        &mut self,
        surface_id: SurfaceId,
        committed: CommittedNode,
        pixel_updates: &mut HashMap<ObjectId, Vec<u8>>,
    ) {
        let object_id = committed.node.object_id;
        if self.layer_for(&object_id).is_none() {
            if let Some(BufferAssignment::NewBuffer(buffer)) = committed.assignment {
                buffer.release();
            }
            warn!(?surface_id, surface = ?object_id, "could not allocate a surface layer id");
            return;
        }
        let cached = self
            .buffers
            .get_mut(&object_id)
            .expect("layer was just inserted");

        match committed.assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let imported = import_buffer(
                    &committed.node.surface,
                    &buffer,
                    committed.buffer_scale,
                    committed.buffer_transform,
                );
                buffer.release();
                cached.client_mapped = true;
                match imported {
                    Ok(imported) => {
                        cached.metadata = Some(imported.metadata);
                        cached.view = Some(imported.view);
                        cached.opaque = imported.opaque;
                        pixel_updates.insert(object_id, imported.pixels);
                    }
                    Err(error) => {
                        cached.metadata = None;
                        cached.view = None;
                        warn!(%error, ?surface_id, surface = ?object_id, "could not import a mapped client buffer");
                    }
                }
            }
            Some(BufferAssignment::Removed) => {
                cached.metadata = None;
                cached.view = None;
                cached.client_mapped = false;
            }
            None => {
                let Some(previous) = cached.metadata else {
                    return;
                };
                let metadata = match checked_buffer_scale(committed.buffer_scale) {
                    Ok(scale) => SurfaceBufferMetadata {
                        scale,
                        transform: committed.buffer_transform,
                        ..previous
                    },
                    Err(error) => {
                        warn!(%error, ?surface_id, surface = ?object_id, "ignored an invalid client surface scale");
                        return;
                    }
                };
                match with_states(&committed.node.surface, |states| {
                    surface_content_view(states, metadata)
                }) {
                    Ok(view) => {
                        cached.metadata = Some(metadata);
                        cached.view = Some(view);
                    }
                    Err(error) => {
                        warn!(%error, ?surface_id, surface = ?object_id, "ignored an invalid client surface view");
                    }
                }
            }
        }
    }

    fn layer_for(&mut self, object_id: &ObjectId) -> Option<SurfaceLayerId> {
        if let Some(buffer) = self.buffers.get(object_id) {
            return Some(buffer.layer);
        }
        let raw = self.next_layer_id?;
        self.next_layer_id = raw.checked_add(1);
        let layer = SurfaceLayerId::new(raw);
        self.buffers.insert(
            object_id.clone(),
            CachedSurfaceBuffer {
                layer,
                metadata: None,
                view: None,
                opaque: false,
                client_mapped: false,
            },
        );
        Some(layer)
    }

    fn snapshot(
        &self,
        root_id: ObjectId,
        mut pixel_updates: HashMap<ObjectId, Vec<u8>>,
    ) -> SurfaceTreeSnapshot {
        let client_mapped = self
            .buffers
            .get(&root_id)
            .is_some_and(|buffer| buffer.client_mapped);
        let root_index = self.nodes.iter().position(|node| node.object_id == root_id);
        let (root, surface_origin) = self
            .placement(&root_id, (0, 0).into())
            .map(|mut root| {
                let (view, origin) = crop_root_view(root.view, self.root_geometry);
                root.view = view;
                (Some(root), origin)
            })
            .unwrap_or((None, bevy::math::Vec2::ZERO));
        let overlays = root_index
            .map(|index| {
                self.nodes[index + 1..]
                    .iter()
                    .filter(|node| self.protocol_visible(node))
                    .filter_map(|node| self.placement(&node.object_id, node.position))
                    .map(|mut placement| {
                        placement.position -= surface_origin;
                        placement
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut buffers = self
            .nodes
            .iter()
            .filter_map(|node| {
                let cached = self.buffers.get(&node.object_id)?;
                let metadata = cached.metadata?;
                cached.view?;
                Some(SurfaceBufferUpdate {
                    layer: cached.layer,
                    width: metadata.width,
                    height: metadata.height,
                    bgra_pixels: pixel_updates.remove(&node.object_id),
                    opaque: cached.opaque,
                })
            })
            .collect::<Vec<_>>();
        buffers.sort_unstable_by_key(|buffer| buffer.layer.raw());
        SurfaceTreeSnapshot {
            client_mapped,
            surface_origin,
            root,
            overlays,
            buffers,
        }
    }

    fn placement(
        &self,
        object_id: &ObjectId,
        position: Point<i32, Logical>,
    ) -> Option<SurfaceLayerPlacement> {
        let cached = self.buffers.get(object_id)?;
        let view = cached.view?;
        Some(SurfaceLayerPlacement {
            layer: cached.layer,
            position: (position.x as f32, position.y as f32).into(),
            view,
        })
    }

    fn protocol_visible(&self, node: &TreeNode) -> bool {
        let mut current = Some(node.object_id.clone());
        while let Some(object_id) = current {
            if !self
                .buffers
                .get(&object_id)
                .is_some_and(|buffer| buffer.client_mapped)
            {
                return false;
            }
            current = self
                .nodes
                .iter()
                .find(|candidate| candidate.object_id == object_id)
                .and_then(|candidate| candidate.parent.clone());
        }
        true
    }
}

fn crop_root_view(
    view: SurfaceContentView,
    geometry: Option<Rectangle<i32, Logical>>,
) -> (SurfaceContentView, bevy::math::Vec2) {
    let Some(geometry) = geometry else {
        return (view, bevy::math::Vec2::ZERO);
    };
    let root_width = f64::from(view.logical_width);
    let root_height = f64::from(view.logical_height);
    let left = f64::from(geometry.loc.x).max(0.0);
    let top = f64::from(geometry.loc.y).max(0.0);
    let right = (f64::from(geometry.loc.x) + f64::from(geometry.size.w)).min(root_width);
    let bottom = (f64::from(geometry.loc.y) + f64::from(geometry.size.h)).min(root_height);
    if right <= left || bottom <= top {
        return (view, bevy::math::Vec2::ZERO);
    }
    if left == 0.0 && top == 0.0 && right == root_width && bottom == root_height {
        return (view, bevy::math::Vec2::ZERO);
    }

    let source_left = f64::from(view.source_x);
    let source_top = f64::from(view.source_y);
    let source_right = f64::from(view.source_x + view.source_width);
    let source_bottom = f64::from(view.source_y + view.source_height);
    let scale_x = f64::from(view.source_width) / root_width;
    let scale_y = f64::from(view.source_height) / root_height;
    let cropped_left = (source_left + left * scale_x).clamp(source_left, source_right);
    let cropped_top = (source_top + top * scale_y).clamp(source_top, source_bottom);
    let cropped_right = (source_left + right * scale_x).clamp(cropped_left, source_right);
    let cropped_bottom = (source_top + bottom * scale_y).clamp(cropped_top, source_bottom);
    let source_x = cropped_left as f32;
    let source_y = cropped_top as f32;
    let source_width = ((cropped_right - cropped_left) as f32)
        .min((view.source_x + view.source_width - source_x).max(0.0));
    let source_height = ((cropped_bottom - cropped_top) as f32)
        .min((view.source_y + view.source_height - source_y).max(0.0));
    if source_width <= 0.0 || source_height <= 0.0 {
        return (view, bevy::math::Vec2::ZERO);
    }

    (
        SurfaceContentView {
            source_x,
            source_y,
            source_width,
            source_height,
            logical_width: (right - left) as f32,
            logical_height: (bottom - top) as f32,
        },
        bevy::math::Vec2::new(left as f32, top as f32),
    )
}

pub(super) fn owning_root(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }
    root
}

pub(super) fn collect_surfaces(root: &WlSurface) -> Vec<WlSurface> {
    let mut surfaces = Vec::new();
    with_surface_tree_upward(
        root,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |surface, _, _| surfaces.push(surface.clone()),
        |_, _, _| true,
    );
    surfaces
}

pub(super) const fn should_drain_callbacks(client_mapped: bool, _displayable: bool) -> bool {
    client_mapped
}

fn surface_offset(states: &smithay::wayland::compositor::SurfaceData) -> Point<i32, Logical> {
    if states.role != Some(SUBSURFACE_ROLE) {
        return (0, 0).into();
    }
    states
        .cached_state
        .get::<SubsurfaceCachedState>()
        .current()
        .location
}

struct ImportedBuffer {
    metadata: SurfaceBufferMetadata,
    view: SurfaceContentView,
    pixels: Vec<u8>,
    opaque: bool,
}

fn import_buffer(
    surface: &WlSurface,
    buffer: &WlBuffer,
    buffer_scale: i32,
    buffer_transform: wl_output::Transform,
) -> anyhow::Result<ImportedBuffer> {
    let copied = copy_shm_buffer(buffer)?;
    let metadata = SurfaceBufferMetadata {
        width: copied.width,
        height: copied.height,
        scale: checked_buffer_scale(buffer_scale)?,
        transform: buffer_transform,
    };
    let view = with_states(surface, |states| surface_content_view(states, metadata))?;
    Ok(ImportedBuffer {
        metadata,
        view,
        pixels: copied.bgra_pixels,
        opaque: copied.opaque,
    })
}

#[cfg(test)]
mod tests {
    use super::{crop_root_view, should_drain_callbacks};
    use crate::surface::SurfaceContentView;
    use smithay::utils::Rectangle;

    #[test]
    fn mapped_clients_keep_frame_callbacks_flowing_after_import_failure() {
        assert!(should_drain_callbacks(true, false));
        assert!(!should_drain_callbacks(false, true));
    }

    #[test]
    fn identity_window_geometry_preserves_the_view_exactly() {
        let view = SurfaceContentView {
            source_x: 7.0,
            source_y: 9.0,
            source_width: 1919.0,
            source_height: 1079.0,
            logical_width: 853.0,
            logical_height: 479.0,
        };

        let (cropped, origin) =
            crop_root_view(view, Some(Rectangle::new((0, 0).into(), (853, 479).into())));

        assert_eq!(cropped, view);
        assert_eq!(origin, bevy::math::Vec2::ZERO);
    }

    #[test]
    fn window_geometry_crops_scaled_source_and_records_surface_origin() {
        let view = SurfaceContentView {
            source_x: 4.0,
            source_y: 8.0,
            source_width: 1600.0,
            source_height: 1200.0,
            logical_width: 800.0,
            logical_height: 600.0,
        };

        let (cropped, origin) = crop_root_view(
            view,
            Some(Rectangle::new((20, 30).into(), (760, 540).into())),
        );

        assert_eq!(origin, bevy::math::Vec2::new(20.0, 30.0));
        assert_eq!(cropped.source_x, 44.0);
        assert_eq!(cropped.source_y, 68.0);
        assert_eq!(cropped.source_width, 1520.0);
        assert_eq!(cropped.source_height, 1080.0);
        assert_eq!(cropped.logical_width, 760.0);
        assert_eq!(cropped.logical_height, 540.0);
    }
}
