//! Lock-safe import and protocol-neutral snapshots of one Wayland surface tree.

use std::collections::{HashMap, HashSet};

use anyhow::Context;
use smithay::{
    backend::allocator::{Buffer, format::has_alpha},
    reexports::wayland_server::{
        Resource,
        backend::ObjectId,
        protocol::{wl_buffer::WlBuffer, wl_output, wl_surface::WlSurface},
    },
    utils::{IsAlive, Logical, Point, Rectangle},
    wayland::{
        compositor::{
            BufferAssignment, RectangleKind, RegionAttributes, SUBSURFACE_ROLE,
            SubsurfaceCachedState, SurfaceAttributes, TraversalAction, get_parent,
            is_sync_subsurface, with_states, with_surface_tree_upward,
        },
        dmabuf::get_dmabuf,
        shell::xdg::SurfaceCachedState as XdgSurfaceCachedState,
    },
};
use tracing::warn;

use crate::surface::{
    LogicalPoint, LogicalSize, SurfaceContentView, SurfaceId, SurfaceInputPlacement,
    SurfaceInputRect, SurfaceLayerId, SurfaceLayerPlacement, SurfaceWindowGeometry,
};

use crate::dmabuf::PendingDmabufFrame;

use super::dmabuf::DmabufReleaseStore;
use super::shm::{
    SurfaceBufferMetadata, checked_buffer_scale, copy_shm_buffer, surface_content_view,
};

#[derive(Debug)]
pub enum PendingSurfaceBufferContent {
    Retained,
    ShmPixels(Vec<u8>),
    ImportedDmabuf(PendingDmabufFrame),
}

#[derive(Debug)]
pub struct PendingSurfaceBufferUpdate {
    pub layer: SurfaceLayerId,
    pub width: u32,
    pub height: u32,
    pub content: PendingSurfaceBufferContent,
    pub opaque: bool,
}

#[derive(Debug)]
pub struct PendingSurfaceTreeSnapshot {
    pub client_mapped: bool,
    pub root: Option<SurfaceLayerPlacement>,
    pub window_geometry: Option<SurfaceWindowGeometry>,
    pub overlays: Vec<SurfaceLayerPlacement>,
    pub inputs: Vec<SurfaceInputPlacement>,
    pub buffers: Vec<PendingSurfaceBufferUpdate>,
}

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
    input_region: Option<RegionAttributes>,
    input_rects: Vec<Rectangle<i32, Logical>>,
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
    input_region: Option<RegionAttributes>,
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
        releases: &mut DmabufReleaseStore,
    ) -> PendingSurfaceTreeSnapshot {
        let _update_span = tracing::trace_span!(
            target: crate::PROFILE_TARGET,
            "smithay_surface_tree_update"
        )
        .entered();

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
                    input_region: current.input_region.clone(),
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

        let mut content_updates = HashMap::new();
        for committed in committed {
            self.apply_commit(surface_id, committed, releases, &mut content_updates);
        }
        self.refresh_input_rects(&root.id());
        self.snapshot(root.id(), content_updates)
    }

    pub(super) fn remove_surface(
        &mut self,
        root: &WlSurface,
        removed: &WlSurface,
    ) -> PendingSurfaceTreeSnapshot {
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

    fn apply_commit(
        &mut self,
        surface_id: SurfaceId,
        committed: CommittedNode,
        releases: &mut DmabufReleaseStore,
        content_updates: &mut HashMap<ObjectId, PendingSurfaceBufferContent>,
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
        cached.input_region = committed.input_region;

        match committed.assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let imported = import_buffer(
                    &committed.node.surface,
                    &buffer,
                    committed.buffer_scale,
                    committed.buffer_transform,
                    releases,
                );
                cached.client_mapped = true;
                match imported {
                    Ok(imported) => {
                        cached.metadata = Some(imported.metadata);
                        cached.view = Some(imported.view);
                        cached.opaque = imported.opaque;
                        content_updates.insert(object_id, imported.content);
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
                input_region: None,
                input_rects: Vec::new(),
            },
        );
        Some(layer)
    }

    fn snapshot(
        &self,
        root_id: ObjectId,
        mut content_updates: HashMap<ObjectId, PendingSurfaceBufferContent>,
    ) -> PendingSurfaceTreeSnapshot {
        let _snapshot_span = tracing::trace_span!(
            target: crate::PROFILE_TARGET,
            "smithay_surface_tree_snapshot"
        )
        .entered();

        let client_mapped = self
            .buffers
            .get(&root_id)
            .is_some_and(|buffer| buffer.client_mapped);
        let root_index = self.nodes.iter().position(|node| node.object_id == root_id);
        let (root, window_geometry) = self
            .placement(&root_id, (0, 0).into())
            .map(|root| {
                let (view, origin) = crop_root_view(root.view, self.root_geometry);
                (Some(root), Some(SurfaceWindowGeometry { origin, view }))
            })
            .unwrap_or((None, None));
        let overlays = root_index
            .map(|index| {
                self.nodes[index + 1..]
                    .iter()
                    .filter(|node| self.protocol_visible(node))
                    .filter_map(|node| self.placement(&node.object_id, node.position))
                    .collect()
            })
            .unwrap_or_default();
        let inputs = root_index
            .map(|index| {
                self.nodes[index..]
                    .iter()
                    .filter(|node| self.protocol_visible(node))
                    .filter_map(|node| self.input_placement(node))
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
                Some(PendingSurfaceBufferUpdate {
                    layer: cached.layer,
                    width: metadata.width,
                    height: metadata.height,
                    content: content_updates
                        .remove(&node.object_id)
                        .unwrap_or(PendingSurfaceBufferContent::Retained),
                    opaque: cached.opaque,
                })
            })
            .collect::<Vec<_>>();
        buffers.sort_unstable_by_key(|buffer| buffer.layer.raw());
        PendingSurfaceTreeSnapshot {
            client_mapped,
            root,
            window_geometry,
            overlays,
            inputs,
            buffers,
        }
    }

    fn input_placement(&self, node: &TreeNode) -> Option<SurfaceInputPlacement> {
        let cached = self.buffers.get(&node.object_id)?;
        cached.view?;
        Some(SurfaceInputPlacement {
            layer: cached.layer,
            position: LogicalPoint::new(node.position.x as f32, node.position.y as f32),
            regions: cached
                .input_rects
                .iter()
                .map(|rectangle| SurfaceInputRect {
                    position: LogicalPoint::new(rectangle.loc.x as f32, rectangle.loc.y as f32),
                    size: LogicalSize::new(rectangle.size.w as f32, rectangle.size.h as f32),
                })
                .collect(),
        })
    }

    fn refresh_input_rects(&mut self, root_id: &ObjectId) {
        for node in &self.nodes {
            let Some(cached) = self.buffers.get_mut(&node.object_id) else {
                continue;
            };
            let Some(view) = cached.view else {
                cached.input_rects.clear();
                continue;
            };
            let bounds = logical_view_bounds(view);
            cached.input_rects = match &cached.input_region {
                Some(region) => effective_region(region, bounds),
                None if &node.object_id == root_id => {
                    vec![displayed_root_bounds(view, self.root_geometry)]
                }
                None => vec![bounds],
            };
        }
    }

    pub(super) fn input_surface(
        &self,
        layer: SurfaceLayerId,
        local: Point<f64, Logical>,
    ) -> Option<WlSurface> {
        let local = local.to_i32_floor();
        let node = self.nodes.iter().find(|node| {
            self.buffers.get(&node.object_id).is_some_and(|cached| {
                cached.layer == layer
                    && cached.client_mapped
                    && cached
                        .input_rects
                        .iter()
                        .any(|rectangle| rectangle.contains(local))
            })
        })?;
        node.surface.alive().then(|| node.surface.clone())
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

fn logical_view_bounds(view: SurfaceContentView) -> Rectangle<i32, Logical> {
    Rectangle::new(
        (0, 0).into(),
        (
            view.logical_width.ceil() as i32,
            view.logical_height.ceil() as i32,
        )
            .into(),
    )
}

fn displayed_root_bounds(
    view: SurfaceContentView,
    geometry: Option<Rectangle<i32, Logical>>,
) -> Rectangle<i32, Logical> {
    let (displayed, origin) = crop_root_view(view, geometry);
    Rectangle::new(
        (origin.x as i32, origin.y as i32).into(),
        (
            displayed.logical_width.ceil() as i32,
            displayed.logical_height.ceil() as i32,
        )
            .into(),
    )
}

fn effective_region(
    region: &RegionAttributes,
    bounds: Rectangle<i32, Logical>,
) -> Vec<Rectangle<i32, Logical>> {
    let mut effective = Vec::new();
    for (kind, rectangle) in &region.rects {
        let Some(rectangle) = bounds.intersection(*rectangle) else {
            continue;
        };
        match kind {
            RectangleKind::Add => effective.push(rectangle),
            RectangleKind::Subtract => {
                effective = Rectangle::subtract_rects_many(effective, [rectangle]);
            }
        }
    }
    effective
}

fn crop_root_view(
    view: SurfaceContentView,
    geometry: Option<Rectangle<i32, Logical>>,
) -> (SurfaceContentView, LogicalPoint) {
    let Some(geometry) = geometry else {
        return (view, LogicalPoint::ZERO);
    };
    let root_width = f64::from(view.logical_width);
    let root_height = f64::from(view.logical_height);
    let left = f64::from(geometry.loc.x).max(0.0);
    let top = f64::from(geometry.loc.y).max(0.0);
    let right = (f64::from(geometry.loc.x) + f64::from(geometry.size.w)).min(root_width);
    let bottom = (f64::from(geometry.loc.y) + f64::from(geometry.size.h)).min(root_height);
    if right <= left || bottom <= top {
        return (view, LogicalPoint::ZERO);
    }
    if left == 0.0 && top == 0.0 && right == root_width && bottom == root_height {
        return (view, LogicalPoint::ZERO);
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
        return (view, LogicalPoint::ZERO);
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
        LogicalPoint::new(left as f32, top as f32),
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
    content: PendingSurfaceBufferContent,
    opaque: bool,
}

fn import_buffer(
    surface: &WlSurface,
    buffer: &WlBuffer,
    buffer_scale: i32,
    buffer_transform: wl_output::Transform,
    releases: &mut DmabufReleaseStore,
) -> anyhow::Result<ImportedBuffer> {
    if let Ok(dmabuf) = get_dmabuf(buffer).cloned() {
        let imported = (|| {
            let size = dmabuf.size();
            let width = u32::try_from(size.w).context("negative DMA-BUF width")?;
            let height = u32::try_from(size.h).context("negative DMA-BUF height")?;
            let metadata = SurfaceBufferMetadata {
                width,
                height,
                scale: checked_buffer_scale(buffer_scale)?,
                transform: buffer_transform,
            };
            let view = with_states(surface, |states| surface_content_view(states, metadata))?;
            let release = releases
                .register(buffer.clone())
                .context("DMA-BUF release identity space is exhausted")?;
            Ok(ImportedBuffer {
                metadata,
                view,
                content: PendingSurfaceBufferContent::ImportedDmabuf(PendingDmabufFrame::new(
                    dmabuf.clone(),
                    release,
                )),
                opaque: !has_alpha(dmabuf.format().code),
            })
        })();
        if imported.is_err() {
            buffer.release();
        }
        return imported;
    }

    let copied = match copy_shm_buffer(buffer) {
        Ok(copied) => copied,
        Err(error) => {
            buffer.release();
            return Err(error);
        }
    };
    buffer.release();
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
        content: PendingSurfaceBufferContent::ShmPixels(copied.bgra_pixels),
        opaque: copied.opaque,
    })
}

#[cfg(test)]
mod tests {
    use super::{crop_root_view, displayed_root_bounds, effective_region};
    use crate::surface::{LogicalPoint, SurfaceContentView};
    use smithay::{
        utils::Rectangle,
        wayland::compositor::{RectangleKind, RegionAttributes},
    };

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
        assert_eq!(origin, LogicalPoint::ZERO);
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

        assert_eq!(origin, LogicalPoint::new(20.0, 30.0));
        assert_eq!(cropped.source_x, 44.0);
        assert_eq!(cropped.source_y, 68.0);
        assert_eq!(cropped.source_width, 1520.0);
        assert_eq!(cropped.source_height, 1080.0);
        assert_eq!(cropped.logical_width, 760.0);
        assert_eq!(cropped.logical_height, 540.0);
    }

    #[test]
    fn ordered_input_region_subtraction_is_clipped_to_the_surface() {
        let region = RegionAttributes {
            rects: vec![
                (
                    RectangleKind::Add,
                    Rectangle::new((-10, -10).into(), (120, 120).into()),
                ),
                (
                    RectangleKind::Subtract,
                    Rectangle::new((25, 25).into(), (50, 50).into()),
                ),
            ],
        };

        let effective = effective_region(&region, Rectangle::new((0, 0).into(), (100, 100).into()));

        assert!(
            effective
                .iter()
                .all(|rectangle| !rectangle.contains((50, 50)))
        );
        assert!(
            effective
                .iter()
                .any(|rectangle| rectangle.contains((10, 10)))
        );
        assert!(effective.iter().all(|rectangle| {
            rectangle.loc.x >= 0
                && rectangle.loc.y >= 0
                && rectangle.loc.x + rectangle.size.w <= 100
                && rectangle.loc.y + rectangle.size.h <= 100
        }));
    }

    #[test]
    fn explicit_root_input_can_extend_outside_the_displayed_window_geometry() {
        let bounds = Rectangle::new((0, 0).into(), (688, 528).into());
        let geometry = Rectangle::new((24, 21).into(), (640, 480).into());
        let explicit = RegionAttributes {
            rects: vec![(
                RectangleKind::Add,
                Rectangle::new((14, 11).into(), (660, 500).into()),
            )],
        };

        let regions = effective_region(&explicit, bounds);
        let implicit = displayed_root_bounds(
            SurfaceContentView {
                source_x: 0.0,
                source_y: 0.0,
                source_width: 688.0,
                source_height: 528.0,
                logical_width: 688.0,
                logical_height: 528.0,
            },
            Some(geometry),
        );

        assert!(regions.iter().any(|region| region.contains((14, 11))));
        assert!(!implicit.contains((14, 11)));
    }
}
