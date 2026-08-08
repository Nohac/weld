//! Bevy-facing compositor state and application-surface composition.
//!
//! Smithay feeds this module owned lifecycle events and pixel data. Compositor
//! plugins operate on [`AppWindow`] and [`SurfaceNode`]; the provisional Bevy
//! [`ImageNode`] backing and its GPU upload lifecycle remain internal.

use std::collections::{HashMap, VecDeque};

use bevy::{
    app::{App, Plugin, PreUpdate},
    asset::{Assets, Handle, RenderAssetUsages},
    color::Color,
    ecs::{
        component::Component, entity::Entity, resource::Resource, schedule::IntoScheduleConfigs,
        system::Res, world::World,
    },
    image::Image,
    math::Rect,
    picking::PickingSystems,
    prelude::{ImageNode, NodeImageMode, px},
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    ui::{
        BorderRadius, BoxShadow, Display, GlobalZIndex, Node, Overflow, PositionType,
        UiTargetCamera, Val,
    },
};
use tracing::warn;

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

/// A client surface that composes as an ordinary Bevy UI primitive.
///
/// Plugins should decorate or arrange this component's entity rather than
/// depending on the internal [`ImageNode`] used by the initial SHM path.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceNode {
    pub surface: SurfaceId,
}

/// An owned frame copied from a client buffer at the Smithay boundary.
#[derive(Debug)]
pub(crate) struct SurfaceFrame {
    pub width: u32,
    pub height: u32,
    pub view: SurfaceContentView,
    pub bgra_pixels: Vec<u8>,
    pub opaque: bool,
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
    Mapped {
        surface: SurfaceId,
    },
    Frame {
        surface: SurfaceId,
        frame: SurfaceFrame,
    },
    ViewChanged {
        surface: SurfaceId,
        view: SurfaceContentView,
    },
    Unmapped {
        surface: SurfaceId,
    },
    Destroyed {
        surface: SurfaceId,
    },
}

pub(crate) struct SurfaceCompositorPlugin;

impl Plugin for SurfaceCompositorPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SurfaceEventQueue>()
            .init_resource::<SurfaceRegistry>()
            .init_resource::<CompositionAdvance>()
            // Asset change collection and UI measurement happen later in the frame.
            .add_systems(
                PreUpdate,
                apply_host_surface_events
                    .run_if(composition_advance_requested)
                    .before(PickingSystems::Backend),
            );
    }
}

/// Gates client-frame application so image asset events are always followed by
/// render extraction in the same host iteration. Standalone Bevy updates are
/// composition advances unless the Weld host explicitly marks them otherwise.
#[derive(Resource)]
struct CompositionAdvance(bool);

impl Default for CompositionAdvance {
    fn default() -> Self {
        Self(true)
    }
}

fn composition_advance_requested(advance: Res<CompositionAdvance>) -> bool {
    advance.0
}

pub(crate) fn set_composition_advance(world: &mut World, enabled: bool) {
    if let Some(mut advance) = world.get_resource_mut::<CompositionAdvance>() {
        advance.0 = enabled;
    }
}

#[derive(Resource, Clone, Copy)]
pub(crate) struct CompositorCamera(pub Entity);

#[derive(Resource, Default)]
pub(crate) struct SurfaceEventQueue(VecDeque<HostSurfaceEvent>);

impl SurfaceEventQueue {
    pub(crate) fn push(&mut self, event: HostSurfaceEvent) {
        let replaces_last_frame = match (&event, self.0.back()) {
            (
                HostSurfaceEvent::Frame { surface, .. },
                Some(HostSurfaceEvent::Frame {
                    surface: previous, ..
                }),
            ) => surface == previous,
            _ => false,
        };
        if replaces_last_frame {
            self.0.pop_back();
        }
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
    image: Option<Handle<Image>>,
    pixel_size: Option<(u32, u32)>,
    frame_ready: bool,
}

pub(crate) fn enqueue_surface_event(world: &mut World, event: HostSurfaceEvent) {
    let Some(mut events) = world.get_resource_mut::<SurfaceEventQueue>() else {
        warn!("discarded a surface event because the compositor ingress is unavailable");
        return;
    };
    events.push(event);
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
            HostSurfaceEvent::Mapped { surface } => {
                ensure_surface_entity(world, &mut registry, surface);
            }
            HostSurfaceEvent::Frame { surface, frame } => {
                apply_surface_frame(world, &mut registry, surface, frame);
            }
            HostSurfaceEvent::ViewChanged { surface, view } => {
                apply_surface_view(world, &mut registry, surface, view);
            }
            HostSurfaceEvent::Unmapped { surface } => {
                unmap_surface(world, &mut registry, surface);
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

    let Some(camera) = world
        .get_resource::<CompositorCamera>()
        .map(|camera| camera.0)
    else {
        warn!(
            ?surface,
            "discarded a surface event because the compositor camera is unavailable"
        );
        return None;
    };

    let entity = world
        .spawn((AppWindow { surface }, SurfaceNode { surface }))
        .insert(empty_surface_image_node())
        .insert(Node {
            display: Display::None,
            position_type: PositionType::Absolute,
            left: px(80),
            top: px(80),
            overflow: Overflow::clip(),
            border_radius: BorderRadius::all(px(18)),
            ..Default::default()
        })
        .insert(BoxShadow::new(
            Color::srgba(0.0, 0.0, 0.0, 0.55),
            px(0),
            px(12),
            px(2),
            px(24),
        ))
        .insert(GlobalZIndex(0))
        .insert(UiTargetCamera(camera))
        .id();
    registry.0.insert(
        surface,
        SurfaceEntry {
            entity,
            image: None,
            pixel_size: None,
            frame_ready: false,
        },
    );
    Some(entity)
}

fn apply_surface_frame(
    world: &mut World,
    registry: &mut SurfaceRegistry,
    surface: SurfaceId,
    mut frame: SurfaceFrame,
) {
    if !validate_frame(&frame) {
        warn!(
            ?surface,
            width = frame.width,
            height = frame.height,
            "discarded an invalid surface frame"
        );
        return;
    }
    let Some(entity) = ensure_surface_entity(world, registry, surface) else {
        return;
    };
    if !frame.opaque {
        unpremultiply_bgra(&mut frame.bgra_pixels);
    }

    let extent = Extent3d {
        width: frame.width,
        height: frame.height,
        depth_or_array_layers: 1,
    };
    let previous = registry
        .0
        .get(&surface)
        .and_then(|entry| entry.image.clone());
    let handle = {
        let Some(mut images) = world.get_resource_mut::<Assets<Image>>() else {
            warn!(
                ?surface,
                "discarded a surface frame because Bevy image assets are unavailable"
            );
            return;
        };
        if let Some(handle) = previous {
            if let Some(mut image) = images.get_mut(&handle) {
                image.texture_descriptor.size = extent;
                image.data = Some(frame.bgra_pixels);
                handle
            } else {
                images.add(surface_image(extent, frame.bgra_pixels))
            }
        } else {
            images.add(surface_image(extent, frame.bgra_pixels))
        }
    };

    let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
        registry.0.remove(&surface);
        warn!(
            ?surface,
            "discarded a surface frame because its ECS entity disappeared"
        );
        return;
    };
    entity_mut.insert(surface_image_node(handle.clone(), frame.view));
    if let Some(mut node) = entity_mut.get_mut::<Node>() {
        node.display = Display::Flex;
        node.width = px(frame.view.logical_width);
        node.height = px(frame.view.logical_height);
    }
    if let Some(entry) = registry.0.get_mut(&surface) {
        entry.image = Some(handle);
        entry.pixel_size = Some((frame.width, frame.height));
        entry.frame_ready = true;
    }
}

fn apply_surface_view(
    world: &mut World,
    registry: &mut SurfaceRegistry,
    surface: SurfaceId,
    view: SurfaceContentView,
) {
    let Some(entry) = registry.0.get(&surface) else {
        warn!(?surface, "discarded a view change for an unknown surface");
        return;
    };
    let (Some(handle), Some((width, height))) = (entry.image.clone(), entry.pixel_size) else {
        warn!(?surface, "discarded a view change for an unmapped surface");
        return;
    };
    if !validate_view(view, width, height) {
        warn!(?surface, ?view, "discarded an invalid surface view");
        return;
    }
    let entity = entry.entity;
    let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
        registry.0.remove(&surface);
        warn!(
            ?surface,
            "discarded a view change because its ECS entity disappeared"
        );
        return;
    };
    entity_mut.insert(surface_image_node(handle, view));
    if let Some(mut node) = entity_mut.get_mut::<Node>() {
        node.width = px(view.logical_width);
        node.height = px(view.logical_height);
    }
}

fn unmap_surface(world: &mut World, registry: &mut SurfaceRegistry, surface: SurfaceId) {
    let Some(entry) = registry.0.get_mut(&surface) else {
        return;
    };
    if let Some(handle) = entry.image.take()
        && let Some(mut images) = world.get_resource_mut::<Assets<Image>>()
    {
        images.remove(handle.id());
    }
    entry.pixel_size = None;
    entry.frame_ready = false;

    if let Ok(mut entity) = world.get_entity_mut(entry.entity) {
        entity.insert(empty_surface_image_node());
        if let Some(mut node) = entity.get_mut::<Node>() {
            node.display = Display::None;
            node.width = Val::Auto;
            node.height = Val::Auto;
        }
    }
}

fn destroy_surface(world: &mut World, registry: &mut SurfaceRegistry, surface: SurfaceId) {
    let Some(entry) = registry.0.remove(&surface) else {
        return;
    };
    if let Some(handle) = entry.image
        && let Some(mut images) = world.get_resource_mut::<Assets<Image>>()
    {
        images.remove(handle.id());
    }
    if let Ok(entity) = world.get_entity_mut(entry.entity) {
        entity.despawn();
    }
}

fn validate_frame(frame: &SurfaceFrame) -> bool {
    let Some(expected) = frame
        .width
        .checked_mul(frame.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
    else {
        return false;
    };
    frame.width > 0
        && frame.height > 0
        && frame.bgra_pixels.len() == expected
        && validate_view(frame.view, frame.width, frame.height)
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

    use super::*;

    fn test_app() -> (App, Entity) {
        let mut app = App::new();
        app.insert_resource(Assets::<Image>::default())
            .add_plugins(SurfaceCompositorPlugin);
        let camera = app.world_mut().spawn_empty().id();
        app.insert_resource(CompositorCamera(camera));
        (app, camera)
    }

    fn frame(pixel: [u8; 4]) -> SurfaceFrame {
        SurfaceFrame {
            width: 1,
            height: 1,
            view: full_view(1.0, 1.0),
            bgra_pixels: pixel.to_vec(),
            opaque: true,
        }
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

    #[test]
    fn applies_map_and_frame_in_one_update_and_reuses_the_image_handle() {
        let (mut app, _) = test_app();
        let surface = SurfaceId::new(7);
        enqueue_surface_event(app.world_mut(), HostSurfaceEvent::Mapped { surface });
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: frame([3, 2, 1, 255]),
            },
        );
        app.update();

        assert!(has_surface_frame(app.world()));
        let mut query = app
            .world_mut()
            .query::<(&AppWindow, &SurfaceNode, &ImageNode, &Node)>();
        let surfaces = query.iter(app.world()).collect::<Vec<_>>();
        let [(window, node, image_node, layout)] = surfaces.as_slice() else {
            panic!("expected one semantic surface entity");
        };
        assert_eq!(window.surface, surface);
        assert_eq!(node.surface, surface);
        assert_eq!(layout.display, Display::Flex);
        assert_eq!(image_node.image_mode, NodeImageMode::Stretch);
        let first_handle = image_node.image.clone();

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: frame([7, 6, 5, 255]),
            },
        );
        app.update();
        let mut query = app.world_mut().query::<&ImageNode>();
        let handles = query
            .iter(app.world())
            .map(|node| node.image.clone())
            .collect::<Vec<_>>();
        assert_eq!(handles, [first_handle]);
    }

    #[test]
    fn unmap_drops_the_asset_and_destroy_removes_the_entity() {
        let (mut app, _) = test_app();
        let surface = SurfaceId::new(11);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: frame([3, 2, 1, 255]),
            },
        );
        app.update();
        let first_handle = {
            let mut query = app.world_mut().query::<&ImageNode>();
            query
                .single(app.world())
                .expect("surface image should exist")
                .image
                .clone()
        };

        enqueue_surface_event(app.world_mut(), HostSurfaceEvent::Unmapped { surface });
        app.update();
        assert!(!has_surface_frame(app.world()));
        assert!(
            app.world()
                .resource::<Assets<Image>>()
                .get(&first_handle)
                .is_none()
        );

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: frame([7, 6, 5, 255]),
            },
        );
        app.update();
        let second_handle = {
            let mut query = app.world_mut().query::<&ImageNode>();
            query
                .single(app.world())
                .expect("remapped surface image should exist")
                .image
                .clone()
        };
        assert_ne!(first_handle, second_handle);

        enqueue_surface_event(app.world_mut(), HostSurfaceEvent::Destroyed { surface });
        app.update();
        let mut query = app.world_mut().query::<&SurfaceNode>();
        assert_eq!(query.iter(app.world()).count(), 0);
    }

    #[test]
    fn coalesces_only_adjacent_frames_for_the_same_surface() {
        let a = SurfaceId::new(1);
        let b = SurfaceId::new(2);
        let mut events = SurfaceEventQueue::default();
        events.push(HostSurfaceEvent::Frame {
            surface: a,
            frame: frame([1, 1, 1, 255]),
        });
        events.push(HostSurfaceEvent::Frame {
            surface: a,
            frame: frame([2, 2, 2, 255]),
        });
        events.push(HostSurfaceEvent::Frame {
            surface: b,
            frame: frame([3, 3, 3, 255]),
        });
        events.push(HostSurfaceEvent::Frame {
            surface: a,
            frame: frame([4, 4, 4, 255]),
        });
        assert_eq!(events.0.len(), 3);
    }

    #[test]
    fn input_only_advances_keep_the_latest_surface_frame_queued_for_composition() {
        let (mut app, _) = test_app();
        let surface = SurfaceId::new(5);
        set_composition_advance(app.world_mut(), false);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: frame([1, 2, 3, 255]),
            },
        );
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: frame([4, 5, 6, 255]),
            },
        );

        app.update();
        app.update();
        let mut surface_query = app.world_mut().query::<&SurfaceNode>();
        assert_eq!(surface_query.iter(app.world()).count(), 0);

        set_composition_advance(app.world_mut(), true);
        app.update();
        let mut image_query = app.world_mut().query::<&ImageNode>();
        let image_node = image_query
            .single(app.world())
            .expect("latest queued frame should be composed");
        let image = app
            .world()
            .resource::<Assets<Image>>()
            .get(&image_node.image)
            .expect("composed frame should have an image asset");
        assert_eq!(image.data.as_deref(), Some([4, 5, 6, 255].as_slice()));
    }

    #[test]
    fn converts_argb_frames_at_the_ingress_boundary() {
        let (mut app, _) = test_app();
        let surface = SurfaceId::new(13);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: SurfaceFrame {
                    width: 1,
                    height: 1,
                    view: full_view(1.0, 1.0),
                    bgra_pixels: vec![25, 50, 75, 128],
                    opaque: false,
                },
            },
        );
        app.update();

        let image_handle = {
            let mut query = app.world_mut().query::<&ImageNode>();
            query
                .single(app.world())
                .expect("ARGB surface image should exist")
                .image
                .clone()
        };
        let image = app
            .world()
            .resource::<Assets<Image>>()
            .get(&image_handle)
            .expect("ARGB surface asset should exist");
        assert_eq!(image.data.as_deref(), Some([50, 100, 149, 128].as_slice()));
    }

    #[test]
    fn rejects_invalid_frames_without_creating_half_mapped_entities() {
        let (mut app, _) = test_app();
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface: SurfaceId::new(17),
                frame: SurfaceFrame {
                    width: 1,
                    height: 1,
                    view: full_view(1.0, 1.0),
                    bgra_pixels: vec![0; 3],
                    opaque: true,
                },
            },
        );
        app.update();

        let mut query = app.world_mut().query::<&SurfaceNode>();
        assert_eq!(query.iter(app.world()).count(), 0);
    }

    #[test]
    fn sizes_the_node_in_surface_logical_pixels_without_downsampling_the_image() {
        let (mut app, _) = test_app();
        let surface = SurfaceId::new(19);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: SurfaceFrame {
                    width: 1_280,
                    height: 960,
                    view: SurfaceContentView {
                        source_x: 0.0,
                        source_y: 0.0,
                        source_width: 1_280.0,
                        source_height: 960.0,
                        logical_width: 640.0,
                        logical_height: 480.0,
                    },
                    bgra_pixels: vec![0; 1_280 * 960 * 4],
                    opaque: true,
                },
            },
        );
        app.update();

        let mut query = app.world_mut().query::<(&ImageNode, &Node)>();
        let (image_node, node) = query
            .single(app.world())
            .expect("scaled surface should exist");
        assert_eq!(node.width, px(640.0));
        assert_eq!(node.height, px(480.0));
        let image = app
            .world()
            .resource::<Assets<Image>>()
            .get(&image_node.image)
            .expect("scaled surface image should exist");
        assert_eq!(image.texture_descriptor.size.width, 1_280);
        assert_eq!(image.texture_descriptor.size.height, 960);
    }

    #[test]
    fn view_changes_update_crop_and_logical_size_without_replacing_the_image() {
        let (mut app, _) = test_app();
        let surface = SurfaceId::new(23);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: SurfaceFrame {
                    width: 4,
                    height: 4,
                    view: full_view(4.0, 4.0),
                    bgra_pixels: vec![0; 4 * 4 * 4],
                    opaque: true,
                },
            },
        );
        app.update();
        let first_handle = {
            let mut query = app.world_mut().query::<&ImageNode>();
            query
                .single(app.world())
                .expect("surface should exist")
                .image
                .clone()
        };
        let view = SurfaceContentView {
            source_x: 1.0,
            source_y: 0.5,
            source_width: 2.0,
            source_height: 3.0,
            logical_width: 8.0,
            logical_height: 6.0,
        };
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::ViewChanged { surface, view },
        );
        app.update();

        let mut query = app.world_mut().query::<(&ImageNode, &Node)>();
        let (image_node, node) = query
            .single(app.world())
            .expect("updated surface should exist");
        assert_eq!(image_node.image, first_handle);
        assert_eq!(image_node.rect, Some(view.source_rect()));
        assert_eq!(image_node.image_mode, NodeImageMode::Stretch);
        assert_eq!(node.width, px(8.0));
        assert_eq!(node.height, px(6.0));
    }

    #[test]
    fn rejects_an_out_of_bounds_surface_crop() {
        let (mut app, _) = test_app();
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface: SurfaceId::new(29),
                frame: SurfaceFrame {
                    width: 1,
                    height: 1,
                    view: SurfaceContentView {
                        source_x: 0.5,
                        ..full_view(1.0, 1.0)
                    },
                    bgra_pixels: vec![0; 4],
                    opaque: true,
                },
            },
        );
        app.update();

        let mut query = app.world_mut().query::<&SurfaceNode>();
        assert_eq!(query.iter(app.world()).count(), 0);
    }

    #[test]
    fn rejects_nonpositive_logical_surface_sizes() {
        assert!(!validate_view(
            SurfaceContentView {
                logical_width: 0.0,
                ..full_view(1.0, 1.0)
            },
            1,
            1,
        ));
    }

    #[test]
    fn view_changes_do_not_create_unknown_surfaces() {
        let (mut app, _) = test_app();
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::ViewChanged {
                surface: SurfaceId::new(31),
                view: full_view(1.0, 1.0),
            },
        );
        app.update();

        let mut query = app.world_mut().query::<&SurfaceNode>();
        assert_eq!(query.iter(app.world()).count(), 0);
    }

    #[test]
    fn converts_premultiplied_bgra_to_straight_alpha() {
        let mut pixels = [25, 50, 75, 128, 9, 8, 7, 0, 3, 2, 1, 255];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(pixels, [50, 100, 149, 128, 0, 0, 0, 0, 3, 2, 1, 255]);
    }
}
