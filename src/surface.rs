//! Bevy-facing application-surface state and client-content rendering.
//!
//! Smithay feeds this module owned lifecycle events and pixel data. The durable
//! [`AppWindow`] entity contains protocol-neutral state only. Presentation
//! plugins claim that entity separately and render its content through
//! [`SurfaceNode`]; the provisional root and overlay Bevy [`ImageNode`] backing
//! remains internal.

use std::collections::{HashMap, HashSet, VecDeque};

use bevy::{
    app::{App, Plugin, PreUpdate},
    asset::{Assets, Handle, RenderAssetUsages},
    ecs::{
        component::Component,
        entity::Entity,
        hierarchy::ChildOf,
        query::{With, Without},
        resource::Resource,
        schedule::{IntoScheduleConfigs, SystemSet},
        system::Query,
        world::World,
    },
    image::Image,
    math::{Rect, Vec2},
    picking::{Pickable, PickingSystems},
    prelude::{ImageNode, NodeImageMode, px},
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    ui::{Display, Node, PositionType, Val},
};
use tracing::warn;

use crate::composition::composition_advance_requested;

/// Stable compositor identity for one client surface.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SurfaceId(u64);

impl SurfaceId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Semantic application-window state exposed to compositor plugins.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppWindow {
    pub surface: SurfaceId,
}

/// Protocol-neutral state available while an application surface is mapped.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct MappedSurface {
    /// Window-geometry extent exposed to shell layout, excluding client-side
    /// shadows and other invisible root-buffer margins.
    pub logical_size: Vec2,
    pub opaque: bool,
}

/// A client surface that composes as an ordinary Bevy UI primitive.
///
/// Plugins should decorate or arrange this component's entity rather than
/// depending on the internal root [`ImageNode`] and ignored overlay children
/// used by the SHM surface-tree path.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[require(ImageNode, Node, SurfaceGeometryOrigin)]
pub struct SurfaceNode {
    pub surface: SurfaceId,
}

/// Root-surface coordinate represented by the presented node's top-left corner.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct SurfaceGeometryOrigin(pub(crate) Vec2);

/// Protocol-neutral request emitted by ECS policy for the host to apply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceAction {
    Close { surface: SurfaceId },
    Focus { surface: Option<SurfaceId> },
}

#[derive(Resource, Default)]
pub(crate) struct SurfaceActionQueue(VecDeque<SurfaceAction>);

impl SurfaceActionQueue {
    pub(crate) fn push(&mut self, action: SurfaceAction) {
        self.0.push_back(action);
    }
}

/// Internal render backing for one mapped client surface tree.
#[derive(Component, Clone, Debug)]
struct SurfaceContent {
    root: SurfaceLayerContent,
    overlays: Vec<SurfaceLayerContent>,
    surface_origin: Vec2,
}

#[derive(Clone, Debug)]
struct SurfaceLayerContent {
    layer: SurfaceLayerId,
    image: Handle<Image>,
    view: SurfaceContentView,
    position: Vec2,
}

/// Stable protocol-neutral identity for one buffer-bearing surface layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SurfaceLayerId(u64);

impl SurfaceLayerId {
    pub(crate) const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) const fn raw(self) -> u64 {
        self.0
    }
}

/// Owned pixels for a changed tree layer, or metadata retaining an existing image.
#[derive(Debug)]
pub(crate) struct SurfaceBufferUpdate {
    pub layer: SurfaceLayerId,
    pub width: u32,
    pub height: u32,
    pub bgra_pixels: Option<Vec<u8>>,
    pub opaque: bool,
}

/// Placement of one imported layer in root-surface logical coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SurfaceLayerPlacement {
    pub layer: SurfaceLayerId,
    pub position: Vec2,
    pub view: SurfaceContentView,
}

/// Complete visible tree state plus pixel deltas copied at the Smithay boundary.
#[derive(Debug)]
pub(crate) struct SurfaceTreeSnapshot {
    pub client_mapped: bool,
    pub surface_origin: Vec2,
    pub root: Option<SurfaceLayerPlacement>,
    pub overlays: Vec<SurfaceLayerPlacement>,
    pub buffers: Vec<SurfaceBufferUpdate>,
}

impl SurfaceTreeSnapshot {
    fn carry_pending_pixels_from(&mut self, previous: &mut Self) {
        let retained = self
            .buffers
            .iter()
            .map(|buffer| buffer.layer)
            .collect::<HashSet<_>>();
        let mut pending = previous
            .buffers
            .iter_mut()
            .filter(|buffer| retained.contains(&buffer.layer))
            .filter_map(|buffer| {
                buffer
                    .bgra_pixels
                    .take()
                    .map(|pixels| (buffer.layer, pixels))
            })
            .collect::<HashMap<_, _>>();
        for buffer in &mut self.buffers {
            if buffer.bgra_pixels.is_none() {
                buffer.bgra_pixels = pending.remove(&buffer.layer);
            }
        }
    }
}

/// The part of a client buffer displayed by a surface and its logical extent.
///
/// Source coordinates are physical image pixels. Destination coordinates are
/// Wayland surface-logical pixels and therefore also drive Bevy layout and
/// client-local pointer coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SurfaceContentView {
    pub source_x: f32,
    pub source_y: f32,
    pub source_width: f32,
    pub source_height: f32,
    pub logical_width: f32,
    pub logical_height: f32,
}

impl SurfaceContentView {
    fn source_rect(self) -> Rect {
        Rect::from_corners(
            (self.source_x, self.source_y).into(),
            (
                self.source_x + self.source_width,
                self.source_y + self.source_height,
            )
                .into(),
        )
    }
}

/// Owned input translated from the Smithay host into compositor ECS state.
#[derive(Debug)]
pub(crate) enum HostSurfaceEvent {
    Created {
        surface: SurfaceId,
    },
    TreeSnapshot {
        surface: SurfaceId,
        snapshot: SurfaceTreeSnapshot,
    },
    Destroyed {
        surface: SurfaceId,
    },
}

pub(crate) struct SurfacePlugin;

/// Stable ordering points around surface ingress and fallback presentation.
///
/// A specialized presentation plugin may run after [`Self::Ingress`] and
/// before [`Self::FallbackPresentation`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, SystemSet)]
pub(crate) enum SurfaceSystems {
    Ingress,
    FallbackPresentation,
}

impl Plugin for SurfacePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SurfaceEventQueue>()
            .init_resource::<SurfaceActionQueue>()
            .init_resource::<SurfaceRegistry>()
            .configure_sets(
                PreUpdate,
                (
                    SurfaceSystems::Ingress,
                    SurfaceSystems::FallbackPresentation,
                )
                    .chain()
                    .before(PickingSystems::Backend),
            )
            // Asset change collection and UI measurement happen later in the frame.
            .add_systems(
                PreUpdate,
                apply_host_surface_events
                    .run_if(composition_advance_requested)
                    .in_set(SurfaceSystems::Ingress),
            )
            .add_systems(
                PreUpdate,
                sync_surface_nodes
                    .run_if(composition_advance_requested)
                    .after(SurfaceSystems::FallbackPresentation)
                    .before(PickingSystems::Backend),
            );
    }
}

#[derive(Resource, Default)]
pub(crate) struct SurfaceEventQueue(VecDeque<HostSurfaceEvent>);

impl SurfaceEventQueue {
    pub(crate) fn push(&mut self, event: HostSurfaceEvent) {
        let event = match event {
            HostSurfaceEvent::TreeSnapshot {
                surface,
                mut snapshot,
            } => {
                if let Some(HostSurfaceEvent::TreeSnapshot {
                    surface: previous_surface,
                    snapshot: previous,
                }) = self.0.back_mut()
                    && surface == *previous_surface
                {
                    snapshot.carry_pending_pixels_from(previous);
                    *previous = snapshot;
                    return;
                }
                HostSurfaceEvent::TreeSnapshot { surface, snapshot }
            }
            event => event,
        };
        self.0.push_back(event);
    }

    pub(crate) fn drain(&mut self) -> impl Iterator<Item = HostSurfaceEvent> + '_ {
        self.0.drain(..)
    }
}

#[derive(Resource, Default)]
struct SurfaceRegistry(HashMap<SurfaceId, SurfaceEntry>);

struct SurfaceEntry {
    entity: Entity,
    buffers: HashMap<SurfaceLayerId, SurfaceBufferAsset>,
    frame_ready: bool,
}

struct SurfaceBufferAsset {
    image: Handle<Image>,
    pixel_size: (u32, u32),
    opaque: bool,
}

#[derive(Component, Default)]
struct SurfaceOverlayNodes(Vec<(SurfaceLayerId, Entity)>);

#[derive(Component)]
struct SurfaceOverlayNode;

pub(crate) fn enqueue_surface_event(world: &mut World, event: HostSurfaceEvent) {
    let Some(mut events) = world.get_resource_mut::<SurfaceEventQueue>() else {
        warn!("discarded a surface event because the compositor ingress is unavailable");
        return;
    };
    events.push(event);
}

pub(crate) fn take_surface_actions(world: &mut World) -> Vec<SurfaceAction> {
    world
        .get_resource_mut::<SurfaceActionQueue>()
        .map(|mut actions| actions.0.drain(..).collect())
        .unwrap_or_default()
}

pub(crate) fn has_surface_frame(world: &World) -> bool {
    world
        .get_resource::<SurfaceRegistry>()
        .is_some_and(|registry| registry.0.values().any(|entry| entry.frame_ready))
}

fn apply_host_surface_events(world: &mut World) {
    let events = world
        .get_resource_mut::<SurfaceEventQueue>()
        .map(|mut events| std::mem::take(&mut events.0))
        .unwrap_or_default();
    let mut registry = world
        .remove_resource::<SurfaceRegistry>()
        .unwrap_or_default();

    for event in events {
        match event {
            HostSurfaceEvent::Created { surface } => {
                ensure_surface_entity(world, &mut registry, surface);
            }
            HostSurfaceEvent::TreeSnapshot { surface, snapshot } => {
                apply_surface_tree_snapshot(world, &mut registry, surface, snapshot);
            }
            HostSurfaceEvent::Destroyed { surface } => {
                destroy_surface(world, &mut registry, surface);
            }
        }
    }

    world.insert_resource(registry);
}

fn ensure_surface_entity(
    world: &mut World,
    registry: &mut SurfaceRegistry,
    surface: SurfaceId,
) -> Option<Entity> {
    if let Some(entry) = registry.0.get(&surface)
        && world.get_entity(entry.entity).is_ok()
    {
        return Some(entry.entity);
    }
    registry.0.remove(&surface);

    let entity = world.spawn(AppWindow { surface }).id();
    registry.0.insert(
        surface,
        SurfaceEntry {
            entity,
            buffers: HashMap::new(),
            frame_ready: false,
        },
    );
    Some(entity)
}

fn apply_surface_tree_snapshot(
    world: &mut World,
    registry: &mut SurfaceRegistry,
    surface: SurfaceId,
    mut snapshot: SurfaceTreeSnapshot,
) {
    if !validate_snapshot(&snapshot) {
        warn!(?surface, "discarded an invalid surface tree snapshot");
        return;
    }
    let Some(entity) = ensure_surface_entity(world, registry, surface) else {
        return;
    };

    let retained = snapshot
        .buffers
        .iter()
        .map(|buffer| buffer.layer)
        .collect::<HashSet<_>>();
    let Some(entry) = registry.0.get_mut(&surface) else {
        return;
    };
    let removed = entry
        .buffers
        .keys()
        .copied()
        .filter(|layer| !retained.contains(layer))
        .collect::<Vec<_>>();
    if let Some(mut images) = world.get_resource_mut::<Assets<Image>>() {
        for layer in removed {
            if let Some(asset) = entry.buffers.remove(&layer) {
                images.remove(asset.image.id());
            }
        }
        for buffer in &mut snapshot.buffers {
            let pixel_size = (buffer.width, buffer.height);
            let extent = Extent3d {
                width: buffer.width,
                height: buffer.height,
                depth_or_array_layers: 1,
            };
            if let Some(mut pixels) = buffer.bgra_pixels.take() {
                if !buffer.opaque {
                    unpremultiply_bgra(&mut pixels);
                }
                let image = if let Some(asset) = entry.buffers.get(&buffer.layer)
                    && let Some(mut image) = images.get_mut(&asset.image)
                {
                    image.texture_descriptor.size = extent;
                    image.data = Some(pixels);
                    asset.image.clone()
                } else {
                    images.add(surface_image(extent, pixels))
                };
                entry.buffers.insert(
                    buffer.layer,
                    SurfaceBufferAsset {
                        image,
                        pixel_size,
                        opaque: buffer.opaque,
                    },
                );
            } else if let Some(asset) = entry.buffers.get_mut(&buffer.layer) {
                asset.pixel_size = pixel_size;
                asset.opaque = buffer.opaque;
            }
        }
    } else {
        warn!(
            ?surface,
            "discarded surface pixels because Bevy image assets are unavailable"
        );
        return;
    }

    let content = snapshot
        .client_mapped
        .then_some(())
        .and(snapshot.root)
        .and_then(|root| {
            let root_asset = entry.buffers.get(&root.layer)?;
            validate_view(root.view, root_asset.pixel_size.0, root_asset.pixel_size.1).then(|| {
                let overlays = snapshot
                    .overlays
                    .iter()
                    .filter_map(|placement| {
                        let asset = entry.buffers.get(&placement.layer)?;
                        validate_view(placement.view, asset.pixel_size.0, asset.pixel_size.1).then(
                            || SurfaceLayerContent {
                                layer: placement.layer,
                                image: asset.image.clone(),
                                view: placement.view,
                                position: placement.position,
                            },
                        )
                    })
                    .collect();
                (
                    SurfaceContent {
                        root: SurfaceLayerContent {
                            layer: root.layer,
                            image: root_asset.image.clone(),
                            view: root.view,
                            position: Vec2::ZERO,
                        },
                        overlays,
                        surface_origin: snapshot.surface_origin,
                    },
                    root_asset.opaque,
                )
            })
        });

    let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
        registry.0.remove(&surface);
        warn!(
            ?surface,
            "discarded a surface snapshot because its ECS entity disappeared"
        );
        return;
    };
    if let Some((content, opaque)) = content {
        let logical_size = Vec2::new(
            content.root.view.logical_width,
            content.root.view.logical_height,
        );
        entity_mut.insert((
            content,
            MappedSurface {
                logical_size,
                opaque,
            },
        ));
        entry.frame_ready = true;
    } else {
        entity_mut.remove::<(SurfaceContent, MappedSurface)>();
        entry.frame_ready = false;
    }
}

fn destroy_surface(world: &mut World, registry: &mut SurfaceRegistry, surface: SurfaceId) {
    let Some(entry) = registry.0.remove(&surface) else {
        return;
    };
    if let Some(mut images) = world.get_resource_mut::<Assets<Image>>() {
        for asset in entry.buffers.into_values() {
            images.remove(asset.image.id());
        }
    }
    if let Ok(entity) = world.get_entity_mut(entry.entity) {
        entity.despawn();
    }
}

type SurfaceNodeQuery<'world, 'state> = Query<
    'world,
    'state,
    (
        Entity,
        &'static SurfaceNode,
        &'static mut ImageNode,
        &'static mut Node,
        &'static mut SurfaceGeometryOrigin,
        Option<&'static SurfaceOverlayNodes>,
    ),
    Without<SurfaceOverlayNode>,
>;

fn sync_surface_nodes(
    mut commands: bevy::ecs::system::Commands,
    surfaces: Query<(&AppWindow, &SurfaceContent)>,
    mut nodes: SurfaceNodeQuery,
    mut overlay_nodes: Query<(&mut ImageNode, &mut Node), With<SurfaceOverlayNode>>,
) {
    for (entity, surface_node, mut image_node, mut node, mut surface_origin, existing_overlays) in
        &mut nodes
    {
        let content = surfaces.iter().find_map(|(window, content)| {
            (window.surface == surface_node.surface).then_some(content)
        });
        let Some(content) = content else {
            let empty_image = empty_surface_image_node();
            if node.display != Display::None {
                node.display = Display::None;
                node.width = Val::Auto;
                node.height = Val::Auto;
            }
            if image_node.image != empty_image.image
                || image_node.rect != empty_image.rect
                || image_node.image_mode != empty_image.image_mode
            {
                *image_node = empty_image;
            }
            surface_origin.0 = Vec2::ZERO;
            if let Some(existing) = existing_overlays {
                for (_, overlay) in &existing.0 {
                    commands.entity(*overlay).despawn();
                }
                commands.entity(entity).remove::<SurfaceOverlayNodes>();
            }
            continue;
        };

        surface_origin.0 = content.surface_origin;

        let expected_image = surface_image_node(content.root.image.clone(), content.root.view);
        if image_node.image != expected_image.image
            || image_node.rect != expected_image.rect
            || image_node.image_mode != expected_image.image_mode
        {
            *image_node = expected_image;
        }
        let logical_width = px(content.root.view.logical_width);
        let logical_height = px(content.root.view.logical_height);
        if node.display != Display::Flex
            || node.width != logical_width
            || node.height != logical_height
        {
            node.display = Display::Flex;
            node.width = logical_width;
            node.height = logical_height;
        }

        let mut reusable = existing_overlays
            .map(|overlays| overlays.0.iter().copied().collect::<HashMap<_, _>>())
            .unwrap_or_default();
        let mut ordered = Vec::with_capacity(content.overlays.len());
        let mut tracked = Vec::with_capacity(content.overlays.len());
        for overlay in &content.overlays {
            let expected_image = surface_image_node(overlay.image.clone(), overlay.view);
            let expected_node = overlay_node(overlay);
            let overlay_entity = if let Some(overlay_entity) = reusable.remove(&overlay.layer) {
                if let Ok((mut image_node, mut node)) = overlay_nodes.get_mut(overlay_entity) {
                    if image_node.image != expected_image.image
                        || image_node.rect != expected_image.rect
                        || image_node.image_mode != expected_image.image_mode
                    {
                        *image_node = expected_image;
                    }
                    if *node != expected_node {
                        *node = expected_node;
                    }
                }
                overlay_entity
            } else {
                commands
                    .spawn((
                        SurfaceOverlayNode,
                        Pickable::IGNORE,
                        expected_image,
                        expected_node,
                        ChildOf(entity),
                    ))
                    .id()
            };
            ordered.push(overlay_entity);
            tracked.push((overlay.layer, overlay_entity));
        }
        for overlay in reusable.into_values() {
            commands.entity(overlay).despawn();
        }
        commands
            .entity(entity)
            .replace_children(&ordered)
            .insert(SurfaceOverlayNodes(tracked));
    }
}

fn overlay_node(layer: &SurfaceLayerContent) -> Node {
    Node {
        position_type: PositionType::Absolute,
        left: px(layer.position.x),
        top: px(layer.position.y),
        width: px(layer.view.logical_width),
        height: px(layer.view.logical_height),
        ..Default::default()
    }
}

fn validate_snapshot(snapshot: &SurfaceTreeSnapshot) -> bool {
    let mut layers = HashSet::new();
    snapshot.buffers.iter().all(|buffer| {
        layers.insert(buffer.layer)
            && buffer.width > 0
            && buffer.height > 0
            && buffer.bgra_pixels.as_ref().is_none_or(|pixels| {
                buffer
                    .width
                    .checked_mul(buffer.height)
                    .and_then(|count| count.checked_mul(4))
                    .and_then(|bytes| usize::try_from(bytes).ok())
                    == Some(pixels.len())
            })
    })
}

fn validate_view(view: SurfaceContentView, width: u32, height: u32) -> bool {
    let values = [
        view.source_x,
        view.source_y,
        view.source_width,
        view.source_height,
        view.logical_width,
        view.logical_height,
    ];
    values.into_iter().all(f32::is_finite)
        && view.source_x >= 0.0
        && view.source_y >= 0.0
        && view.source_width > 0.0
        && view.source_height > 0.0
        && view.logical_width > 0.0
        && view.logical_height > 0.0
        && view.source_x + view.source_width <= width as f32
        && view.source_y + view.source_height <= height as f32
}

fn surface_image_node(image: Handle<Image>, view: SurfaceContentView) -> ImageNode {
    ImageNode {
        image,
        rect: Some(view.source_rect()),
        image_mode: NodeImageMode::Stretch,
        ..Default::default()
    }
}

fn empty_surface_image_node() -> ImageNode {
    ImageNode {
        image_mode: NodeImageMode::Stretch,
        ..Default::default()
    }
}

fn surface_image(extent: Extent3d, pixels: Vec<u8>) -> Image {
    Image::new(
        extent,
        TextureDimension::D2,
        pixels,
        TextureFormat::Bgra8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}

/// Converts encoded premultiplied BGRA channels to straight alpha in place.
///
/// Bevy UI uses straight-alpha blending. Dividing before the sRGB sampler
/// decode preserves the current linear premultiplied result, at the cost of
/// unavoidable quantization for very small alpha values.
fn unpremultiply_bgra(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 {
            pixel[..3].fill(0);
            continue;
        }
        for channel in &mut pixel[..3] {
            let straight = (u32::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = straight.min(255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use bevy::app::App;

    use crate::composition::{CompositionPlugin, set_composition_advance};

    use super::*;

    fn test_app() -> App {
        let mut app = App::new();
        app.insert_resource(Assets::<Image>::default())
            .add_plugins((CompositionPlugin, SurfacePlugin));
        app
    }

    fn full_view(width: f32, height: f32) -> SurfaceContentView {
        SurfaceContentView {
            source_x: 0.0,
            source_y: 0.0,
            source_width: width,
            source_height: height,
            logical_width: width,
            logical_height: height,
        }
    }

    fn placement(layer: u64, position: Vec2) -> SurfaceLayerPlacement {
        SurfaceLayerPlacement {
            layer: SurfaceLayerId::new(layer),
            position,
            view: full_view(1.0, 1.0),
        }
    }

    fn buffer(layer: u64, pixel: Option<[u8; 4]>) -> SurfaceBufferUpdate {
        SurfaceBufferUpdate {
            layer: SurfaceLayerId::new(layer),
            width: 1,
            height: 1,
            bgra_pixels: pixel.map(Vec::from),
            opaque: true,
        }
    }

    fn root_snapshot(pixel: Option<[u8; 4]>) -> SurfaceTreeSnapshot {
        SurfaceTreeSnapshot {
            client_mapped: true,
            surface_origin: Vec2::ZERO,
            root: Some(placement(1, Vec2::ZERO)),
            overlays: Vec::new(),
            buffers: vec![buffer(1, pixel)],
        }
    }

    fn snapshot_event(surface: SurfaceId, snapshot: SurfaceTreeSnapshot) -> HostSurfaceEvent {
        HostSurfaceEvent::TreeSnapshot { surface, snapshot }
    }

    #[test]
    fn root_updates_reuse_the_bevy_image() {
        let mut app = test_app();
        let surface = SurfaceId::new(7);
        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(Some([3, 2, 1, 255]))),
        );
        app.update();
        let first_handle = {
            let mut query = app.world_mut().query::<&SurfaceContent>();
            query
                .single(app.world())
                .expect("root image should exist")
                .root
                .image
                .clone()
        };

        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(Some([7, 6, 5, 255]))),
        );
        app.update();
        let mut query = app.world_mut().query::<&SurfaceContent>();
        let content = query
            .single(app.world())
            .expect("updated root should exist");
        assert_eq!(content.root.image, first_handle);
    }

    #[test]
    fn protocol_unmap_retains_copied_buffers_for_remapping() {
        let mut app = test_app();
        let surface = SurfaceId::new(11);
        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(Some([3, 2, 1, 255]))),
        );
        app.update();
        let first_handle = {
            let mut query = app.world_mut().query::<&SurfaceContent>();
            query
                .single(app.world())
                .expect("mapped root should exist")
                .root
                .image
                .clone()
        };

        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(
                surface,
                SurfaceTreeSnapshot {
                    client_mapped: false,
                    surface_origin: Vec2::ZERO,
                    root: None,
                    overlays: Vec::new(),
                    buffers: vec![buffer(1, None)],
                },
            ),
        );
        app.update();
        assert!(!has_surface_frame(app.world()));
        assert!(
            app.world()
                .resource::<Assets<Image>>()
                .get(&first_handle)
                .is_some()
        );

        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(None)),
        );
        app.update();
        let mut query = app.world_mut().query::<&SurfaceContent>();
        let remapped = query.single(app.world()).expect("root should remap");
        assert_eq!(remapped.root.image, first_handle);
    }

    #[test]
    fn coalescing_preserves_unseen_pixels_and_drops_removed_layers() {
        let surface = SurfaceId::new(1);
        let mut events = SurfaceEventQueue::default();
        let mut first = root_snapshot(Some([1, 1, 1, 255]));
        first.buffers.push(buffer(2, Some([2, 2, 2, 255])));
        first.overlays.push(placement(2, Vec2::ZERO));
        events.push(snapshot_event(surface, first));

        let mut next = root_snapshot(None);
        next.buffers.push(buffer(3, Some([3, 3, 3, 255])));
        next.overlays.push(placement(3, Vec2::ZERO));
        events.push(snapshot_event(surface, next));

        let Some(HostSurfaceEvent::TreeSnapshot { snapshot, .. }) = events.0.front() else {
            panic!("adjacent snapshots should merge");
        };
        assert_eq!(events.0.len(), 1);
        assert_eq!(snapshot.buffers.len(), 2);
        assert_eq!(
            snapshot.buffers[0].bgra_pixels.as_deref(),
            Some([1, 1, 1, 255].as_slice())
        );
        assert_eq!(snapshot.buffers[1].layer, SurfaceLayerId::new(3));
    }

    #[test]
    fn input_only_advances_keep_latest_tree_pixels_queued() {
        let mut app = test_app();
        let surface = SurfaceId::new(5);
        set_composition_advance(app.world_mut(), false);
        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(Some([1, 2, 3, 255]))),
        );
        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(Some([4, 5, 6, 255]))),
        );
        app.update();
        app.update();
        let mut mapped = app.world_mut().query::<&MappedSurface>();
        assert_eq!(mapped.iter(app.world()).count(), 0);

        set_composition_advance(app.world_mut(), true);
        app.update();
        let mut content = app.world_mut().query::<&SurfaceContent>();
        let content = content
            .single(app.world())
            .expect("queued root should compose");
        let image = app
            .world()
            .resource::<Assets<Image>>()
            .get(&content.root.image)
            .expect("root asset should exist");
        assert_eq!(image.data.as_deref(), Some([4, 5, 6, 255].as_slice()));
    }

    #[test]
    fn overlays_reuse_entities_and_release_removed_assets() {
        let mut app = test_app();
        let surface = SurfaceId::new(29);
        app.world_mut().spawn((
            SurfaceNode { surface },
            ImageNode::default(),
            Node::default(),
        ));
        let mut snapshot = root_snapshot(Some([1, 1, 1, 255]));
        snapshot.surface_origin = Vec2::new(24.0, 32.0);
        snapshot.buffers.push(buffer(2, Some([2, 2, 2, 255])));
        snapshot.overlays.push(placement(2, Vec2::new(4.0, 5.0)));
        enqueue_surface_event(app.world_mut(), snapshot_event(surface, snapshot));
        app.update();

        let mut origins = app.world_mut().query::<&SurfaceGeometryOrigin>();
        assert_eq!(
            origins
                .single(app.world())
                .expect("surface node should carry its geometry origin")
                .0,
            Vec2::new(24.0, 32.0)
        );

        let first_overlay = {
            let mut query = app
                .world_mut()
                .query_filtered::<Entity, With<SurfaceOverlayNode>>();
            query.single(app.world()).expect("overlay should exist")
        };
        let overlay_image = app
            .world()
            .get::<ImageNode>(first_overlay)
            .expect("overlay should own an image")
            .image
            .clone();

        let mut moved = root_snapshot(None);
        moved.buffers.push(buffer(2, None));
        moved.overlays.push(placement(2, Vec2::new(8.0, 9.0)));
        enqueue_surface_event(app.world_mut(), snapshot_event(surface, moved));
        app.update();
        let mut query = app
            .world_mut()
            .query_filtered::<Entity, With<SurfaceOverlayNode>>();
        assert_eq!(
            query.single(app.world()).expect("overlay should be reused"),
            first_overlay
        );

        enqueue_surface_event(
            app.world_mut(),
            snapshot_event(surface, root_snapshot(None)),
        );
        app.update();
        let mut query = app
            .world_mut()
            .query_filtered::<Entity, With<SurfaceOverlayNode>>();
        assert_eq!(query.iter(app.world()).count(), 0);
        assert!(
            app.world()
                .resource::<Assets<Image>>()
                .get(&overlay_image)
                .is_none()
        );
    }

    #[test]
    fn converts_premultiplied_bgra_to_straight_alpha() {
        let mut pixels = [25, 50, 75, 128, 9, 8, 7, 0, 3, 2, 1, 255];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(pixels, [50, 100, 149, 128, 0, 0, 0, 0, 3, 2, 1, 255]);
    }
}
