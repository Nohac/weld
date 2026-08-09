//! Bevy-facing application-surface state and client-content rendering.
//!
//! Smithay feeds this module owned lifecycle events and pixel data. The durable
//! [`AppWindow`] entity contains protocol-neutral state only. Presentation
//! plugins claim that entity separately and render its content through
//! [`SurfaceNode`]; the provisional Bevy [`ImageNode`] backing remains internal.

use std::collections::{HashMap, VecDeque};

use bevy::{
    app::{App, Plugin, PreUpdate},
    asset::{Assets, Handle, RenderAssetUsages},
    ecs::{
        component::Component,
        entity::Entity,
        resource::Resource,
        schedule::{IntoScheduleConfigs, SystemSet},
        system::Query,
        world::World,
    },
    image::Image,
    math::{Rect, Vec2},
    picking::PickingSystems,
    prelude::{ImageNode, NodeImageMode, px},
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    ui::{Display, Node, Val},
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
    pub logical_size: Vec2,
    pub opaque: bool,
}

/// A client surface that composes as an ordinary Bevy UI primitive.
///
/// Plugins should decorate or arrange this component's entity rather than
/// depending on the internal [`ImageNode`] used by the initial SHM path.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[require(ImageNode, Node)]
pub struct SurfaceNode {
    pub surface: SurfaceId,
}

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

/// Internal render backing for one mapped client surface.
#[derive(Component, Clone, Debug)]
struct SurfaceContent {
    image: Handle<Image>,
    view: SurfaceContentView,
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
    Created {
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

    let entity = world.spawn(AppWindow { surface }).id();
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
    entity_mut.insert((
        SurfaceContent {
            image: handle.clone(),
            view: frame.view,
        },
        MappedSurface {
            logical_size: Vec2::new(frame.view.logical_width, frame.view.logical_height),
            opaque: frame.opaque,
        },
    ));
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
    let (Some(_handle), Some((width, height))) = (entry.image.as_ref(), entry.pixel_size) else {
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
    {
        let Some(mut content) = entity_mut.get_mut::<SurfaceContent>() else {
            warn!(?surface, "discarded a view change without surface content");
            return;
        };
        content.view = view;
    }
    if let Some(mut mapped) = entity_mut.get_mut::<MappedSurface>() {
        mapped.logical_size = Vec2::new(view.logical_width, view.logical_height);
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
        entity.remove::<(SurfaceContent, MappedSurface)>();
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

fn sync_surface_nodes(
    surfaces: Query<(&AppWindow, &SurfaceContent)>,
    mut nodes: Query<(&SurfaceNode, &mut ImageNode, &mut Node)>,
) {
    for (surface_node, mut image_node, mut node) in &mut nodes {
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
            continue;
        };

        let expected_image = surface_image_node(content.image.clone(), content.view);
        if image_node.image != expected_image.image
            || image_node.rect != expected_image.rect
            || image_node.image_mode != expected_image.image_mode
        {
            *image_node = expected_image;
        }
        let logical_width = px(content.view.logical_width);
        let logical_height = px(content.view.logical_height);
        if node.display != Display::Flex
            || node.width != logical_width
            || node.height != logical_height
        {
            node.display = Display::Flex;
            node.width = logical_width;
            node.height = logical_height;
        }
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

    use crate::composition::{CompositionPlugin, set_composition_advance};

    use super::*;

    fn test_app() -> App {
        let mut app = App::new();
        app.insert_resource(Assets::<Image>::default())
            .add_plugins((CompositionPlugin, SurfacePlugin));
        app
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
        let mut app = test_app();
        let surface = SurfaceId::new(7);
        enqueue_surface_event(app.world_mut(), HostSurfaceEvent::Created { surface });
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
            .query::<(&AppWindow, &MappedSurface, &SurfaceContent)>();
        let surfaces = query.iter(app.world()).collect::<Vec<_>>();
        let [(window, mapped, content)] = surfaces.as_slice() else {
            panic!("expected one mapped surface data entity");
        };
        assert_eq!(window.surface, surface);
        assert_eq!(mapped.logical_size, Vec2::ONE);
        let first_handle = content.image.clone();

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: frame([7, 6, 5, 255]),
            },
        );
        app.update();
        let mut query = app.world_mut().query::<&SurfaceContent>();
        let handles = query
            .iter(app.world())
            .map(|content| content.image.clone())
            .collect::<Vec<_>>();
        assert_eq!(handles, [first_handle]);
    }

    #[test]
    fn unmap_drops_the_asset_and_destroy_removes_the_entity() {
        let mut app = test_app();
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
            let mut query = app.world_mut().query::<&SurfaceContent>();
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
        let mut source_query =
            app.world_mut()
                .query::<(&AppWindow, Option<&MappedSurface>, Option<&SurfaceContent>)>();
        let (_, mapped, content) = source_query
            .single(app.world())
            .expect("unmapping should preserve the source entity");
        assert!(mapped.is_none());
        assert!(content.is_none());

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: frame([7, 6, 5, 255]),
            },
        );
        app.update();
        let second_handle = {
            let mut query = app.world_mut().query::<&SurfaceContent>();
            query
                .single(app.world())
                .expect("remapped surface image should exist")
                .image
                .clone()
        };
        assert_ne!(first_handle, second_handle);

        enqueue_surface_event(app.world_mut(), HostSurfaceEvent::Destroyed { surface });
        app.update();
        let mut query = app.world_mut().query::<&AppWindow>();
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
        let mut app = test_app();
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
        let mut surface_query = app.world_mut().query::<&MappedSurface>();
        assert_eq!(surface_query.iter(app.world()).count(), 0);

        set_composition_advance(app.world_mut(), true);
        app.update();
        let mut content_query = app.world_mut().query::<&SurfaceContent>();
        let content = content_query
            .single(app.world())
            .expect("latest queued frame should be composed");
        let image = app
            .world()
            .resource::<Assets<Image>>()
            .get(&content.image)
            .expect("composed frame should have an image asset");
        assert_eq!(image.data.as_deref(), Some([4, 5, 6, 255].as_slice()));
    }

    #[test]
    fn converts_argb_frames_at_the_ingress_boundary() {
        let mut app = test_app();
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
            let mut query = app.world_mut().query::<&SurfaceContent>();
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
        let mut app = test_app();
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

        let mut query = app.world_mut().query::<&AppWindow>();
        assert_eq!(query.iter(app.world()).count(), 0);
    }

    #[test]
    fn sizes_the_node_in_surface_logical_pixels_without_downsampling_the_image() {
        let mut app = test_app();
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

        let mut query = app.world_mut().query::<(&MappedSurface, &SurfaceContent)>();
        let (mapped, content) = query
            .single(app.world())
            .expect("scaled surface should exist");
        assert_eq!(mapped.logical_size, Vec2::new(640.0, 480.0));
        let image = app
            .world()
            .resource::<Assets<Image>>()
            .get(&content.image)
            .expect("scaled surface image should exist");
        assert_eq!(image.texture_descriptor.size.width, 1_280);
        assert_eq!(image.texture_descriptor.size.height, 960);
    }

    #[test]
    fn view_changes_update_crop_and_logical_size_without_replacing_the_image() {
        let mut app = test_app();
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
            let mut query = app.world_mut().query::<&SurfaceContent>();
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

        let mut query = app.world_mut().query::<(&SurfaceContent, &MappedSurface)>();
        let (content, mapped) = query
            .single(app.world())
            .expect("updated surface should exist");
        assert_eq!(content.image, first_handle);
        assert_eq!(content.view, view);
        assert_eq!(mapped.logical_size, Vec2::new(8.0, 6.0));
    }

    #[test]
    fn rejects_an_out_of_bounds_surface_crop() {
        let mut app = test_app();
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

        let mut query = app.world_mut().query::<&AppWindow>();
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
        let mut app = test_app();
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::ViewChanged {
                surface: SurfaceId::new(31),
                view: full_view(1.0, 1.0),
            },
        );
        app.update();

        let mut query = app.world_mut().query::<&AppWindow>();
        assert_eq!(query.iter(app.world()).count(), 0);
    }

    #[test]
    fn converts_premultiplied_bgra_to_straight_alpha() {
        let mut pixels = [25, 50, 75, 128, 9, 8, 7, 0, 3, 2, 1, 255];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(pixels, [50, 100, 149, 128, 0, 0, 0, 0, 3, 2, 1, 255]);
    }
}
