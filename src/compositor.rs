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
        world::World,
    },
    image::Image,
    picking::PickingSystems,
    prelude::{ImageNode, px},
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    ui::{
        BorderRadius, BoxShadow, Display, GlobalZIndex, Node, Overflow, PositionType,
        UiTargetCamera,
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
    pub bgra_pixels: Vec<u8>,
    pub opaque: bool,
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
            // Asset change collection and UI measurement happen later in the frame.
            .add_systems(
                PreUpdate,
                apply_host_surface_events.before(PickingSystems::Backend),
            );
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
        .insert(ImageNode::default())
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
    let mut replace_image_node = previous.is_none();
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
                replace_image_node = true;
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
    if replace_image_node {
        entity_mut.insert(ImageNode::new(handle.clone()));
    }
    if let Some(mut node) = entity_mut.get_mut::<Node>() {
        node.display = Display::Flex;
    }
    if let Some(entry) = registry.0.get_mut(&surface) {
        entry.image = Some(handle);
        entry.frame_ready = true;
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
    entry.frame_ready = false;

    if let Ok(mut entity) = world.get_entity_mut(entry.entity) {
        entity.insert(ImageNode::default());
        if let Some(mut node) = entity.get_mut::<Node>() {
            node.display = Display::None;
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
    frame.width > 0 && frame.height > 0 && frame.bgra_pixels.len() == expected
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
            bgra_pixels: pixel.to_vec(),
            opaque: true,
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
    fn converts_premultiplied_bgra_to_straight_alpha() {
        let mut pixels = [25, 50, 75, 128, 9, 8, 7, 0, 3, 2, 1, 255];
        unpremultiply_bgra(&mut pixels);
        assert_eq!(pixels, [50, 100, 149, 128, 0, 0, 0, 0, 3, 2, 1, 255]);
    }
}
