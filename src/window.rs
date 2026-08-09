//! Default application-window presentation built from project-owned ECS state.

use std::{
    collections::{VecDeque, hash_map::RandomState},
    hash::BuildHasher,
};

use bevy::{
    app::{App, Plugin, PreUpdate},
    color::Color,
    ecs::{
        component::Component,
        entity::Entity,
        message::MessageWriter,
        observer::On,
        query::Without,
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Commands, Query, Res, ResMut},
        world::World,
    },
    math::{Rot2, UVec2, Vec2},
    picking::{
        Pickable, PickingSystems,
        events::{Click, Drag, Pointer},
        pointer::PointerButton,
    },
    prelude::{
        AlignItems, BackgroundColor, BorderColor, BorderRadius, BoxShadow, Button, ChildOf,
        Children, Display, FlexDirection, GlobalZIndex, ImageNode, JustifyContent, Node, Overflow,
        PositionType, Scene, UiRect, UiTransform, Val, With, percent, px,
    },
    scene::{CommandsSceneExt, bsn, on},
    ui::{UiScale, UiTargetCamera},
    window::RequestRedraw,
};
use tracing::warn;

use crate::compositor::{
    AppWindow, CompositorCamera, MappedSurface, SurfaceId, SurfaceNode, SurfaceSystems,
    composition_advance_requested,
};

const BORDER_WIDTH: f32 = 3.0;
const HEADER_HEIGHT: f32 = 30.0;
const CLOSE_BUTTON_SIZE: f32 = 22.0;

/// A presentation root claiming the primary view of a surface.
///
/// Inserting a second claim displaces the relationship from the old root but
/// does not despawn that root. A future multi-presenter API must either reject
/// replacement or explicitly clean up the displaced presentation.
#[derive(Component, Clone, Copy, Debug)]
#[relationship(relationship_target = PrimaryPresentation)]
pub(crate) struct PresentsSurface(Entity);

/// The one primary presentation currently related to a surface data entity.
#[derive(Component, Debug)]
#[relationship_target(relationship = PresentsSurface, linked_spawn)]
pub(crate) struct PrimaryPresentation(Entity);

#[derive(Component, Clone, Copy, Debug)]
struct DefaultWindow {
    surface: SurfaceId,
}

/// Protocol-neutral action emitted by compositor policy for the host to apply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceAction {
    Close { surface: SurfaceId },
}

#[derive(Resource, Default)]
struct SurfaceActionQueue(VecDeque<SurfaceAction>);

#[derive(Resource)]
struct PlacementRandom(RandomState);

impl Default for PlacementRandom {
    fn default() -> Self {
        Self(RandomState::new())
    }
}

impl PlacementRandom {
    fn samples(&self, surface: SurfaceId) -> Vec2 {
        Vec2::new(
            hash_unit(self.0.hash_one((surface.raw(), 0_u8))),
            hash_unit(self.0.hash_one((surface.raw(), 1_u8))),
        )
    }
}

#[derive(Resource, Clone, Copy, Debug, PartialEq)]
struct OutputGeometry {
    physical_size: UVec2,
    scale_factor: f32,
}

impl OutputGeometry {
    fn new(physical_size: UVec2, scale_factor: f64) -> Self {
        Self {
            physical_size,
            scale_factor: valid_scale_factor(scale_factor),
        }
    }

    fn logical_size(self) -> Vec2 {
        self.physical_size.as_vec2() / self.scale_factor
    }
}

pub(crate) struct DefaultWindowPlugin {
    output_size: UVec2,
    scale_factor: f64,
}

impl DefaultWindowPlugin {
    pub(crate) const fn new(output_size: UVec2, scale_factor: f64) -> Self {
        Self {
            output_size,
            scale_factor,
        }
    }
}

impl Plugin for DefaultWindowPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(OutputGeometry::new(self.output_size, self.scale_factor))
            .init_resource::<PlacementRandom>()
            .init_resource::<SurfaceActionQueue>()
            .add_systems(
                PreUpdate,
                present_default_windows
                    .run_if(composition_advance_requested)
                    .in_set(SurfaceSystems::FallbackPresentation),
            )
            .add_systems(
                PreUpdate,
                sync_default_window_visibility
                    .run_if(composition_advance_requested)
                    .after(SurfaceSystems::FallbackPresentation)
                    .before(PickingSystems::Backend),
            );
    }
}

pub(crate) fn set_output_physical_size(world: &mut World, physical_size: UVec2) {
    if let Some(mut geometry) = world.get_resource_mut::<OutputGeometry>() {
        geometry.physical_size = physical_size;
    }
}

pub(crate) fn set_output_scale_factor(world: &mut World, scale_factor: f64) {
    if let Some(mut geometry) = world.get_resource_mut::<OutputGeometry>() {
        geometry.scale_factor = valid_scale_factor(scale_factor);
    }
}

pub(crate) fn take_surface_actions(world: &mut World) -> Vec<SurfaceAction> {
    world
        .get_resource_mut::<SurfaceActionQueue>()
        .map(|mut actions| actions.0.drain(..).collect())
        .unwrap_or_default()
}

fn present_default_windows(
    mut commands: Commands,
    camera: Option<Res<CompositorCamera>>,
    output: Res<OutputGeometry>,
    random: Res<PlacementRandom>,
    surfaces: Query<(Entity, &AppWindow, &MappedSurface), Without<PrimaryPresentation>>,
) {
    let Some(camera) = camera else {
        if !surfaces.is_empty() {
            warn!("left mapped surfaces unclaimed because the compositor camera is unavailable");
        }
        return;
    };

    for (source, window, mapped) in &surfaces {
        let decorated_size =
            mapped.logical_size + Vec2::new(BORDER_WIDTH * 2.0, HEADER_HEIGHT + BORDER_WIDTH * 2.0);
        let position = random_placement(
            output.logical_size(),
            decorated_size,
            random.samples(window.surface),
        );
        let mut root = commands.spawn_scene(default_window_scene(window.surface, position));
        let root_entity = root.id();
        root.insert((
            PresentsSurface(source),
            DefaultWindow {
                surface: window.surface,
            },
            UiTargetCamera(camera.0),
            GlobalZIndex(0),
        ));
        commands.spawn((
            SurfaceNode {
                surface: window.surface,
            },
            ImageNode::default(),
            Node {
                display: Display::None,
                ..Default::default()
            },
            ChildOf(root_entity),
        ));
    }
}

fn sync_default_window_visibility(
    surfaces: Query<(&AppWindow, Option<&MappedSurface>)>,
    mut windows: Query<(&DefaultWindow, &mut Node)>,
) {
    for (window, mut node) in &mut windows {
        let mapped = surfaces
            .iter()
            .find_map(|(surface, mapped)| (surface.surface == window.surface).then_some(mapped))
            .flatten()
            .is_some();
        let display = if mapped { Display::Flex } else { Display::None };
        if node.display != display {
            node.display = display;
        }
    }
}

fn default_window_scene(surface: SurfaceId, position: Vec2) -> impl Scene {
    bsn! {
        Node {
            position_type: PositionType::Absolute,
            left: px(position.x),
            top: px(position.y),
            flex_direction: FlexDirection::Column,
            border: UiRect::all(px(BORDER_WIDTH)),
            border_radius: BorderRadius::all(px(9)),
            overflow: Overflow::clip(),
        }
        BorderColor::all(Color::srgb(0.28, 0.34, 0.42))
        BackgroundColor(Color::srgb(0.10, 0.12, 0.16))
        BoxShadow::new(
            Color::srgba(0.0, 0.0, 0.0, 0.55),
            px(0),
            px(12),
            px(2),
            px(24),
        )
        on(drag_window)
        Children [
            (
                Node {
                    width: percent(100),
                    height: px(HEADER_HEIGHT),
                    flex_shrink: 0.0,
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::FlexEnd,
                }
                BackgroundColor(Color::srgb(0.14, 0.17, 0.22))
                on(drag_window)
                Children [(
                    Button
                    Node {
                        width: px(CLOSE_BUTTON_SIZE),
                        height: px(CLOSE_BUTTON_SIZE),
                        margin: UiRect::right(px(4)),
                        align_items: AlignItems::Center,
                        justify_content: JustifyContent::Center,
                        border_radius: BorderRadius::MAX,
                    }
                    BackgroundColor(Color::srgb(0.54, 0.16, 0.18))
                    on(close_window(surface))
                    Children [(
                        Pickable::IGNORE
                        Node {
                            width: px(12),
                            height: px(12),
                            position_type: PositionType::Relative,
                        }
                        Children [
                            (
                                Pickable::IGNORE
                                Node {
                                    position_type: PositionType::Absolute,
                                    left: px(0),
                                    top: px(5),
                                    width: px(12),
                                    height: px(2),
                                }
                                UiTransform::from_rotation(Rot2::degrees(45.0))
                                BackgroundColor(Color::WHITE)
                            ),
                            (
                                Pickable::IGNORE
                                Node {
                                    position_type: PositionType::Absolute,
                                    left: px(0),
                                    top: px(5),
                                    width: px(12),
                                    height: px(2),
                                }
                                UiTransform::from_rotation(Rot2::degrees(-45.0))
                                BackgroundColor(Color::WHITE)
                            ),
                        ]
                    )]
                )]
            ),
        ]
    }
}

fn drag_window(
    mut drag: On<Pointer<Drag>>,
    windows: Query<(), With<DefaultWindow>>,
    parents: Query<&ChildOf>,
    mut nodes: Query<&mut Node>,
    ui_scale: Res<UiScale>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if drag.button != PointerButton::Primary || drag.original_event_target() != drag.entity {
        return;
    }
    let window = if windows.contains(drag.entity) {
        drag.entity
    } else {
        let Ok(parent) = parents.get(drag.entity) else {
            return;
        };
        parent.parent()
    };
    let Ok(mut node) = nodes.get_mut(window) else {
        return;
    };
    let Some(position) = dragged_position(node.left, node.top, drag.delta, ui_scale.0) else {
        return;
    };
    node.left = px(position.x);
    node.top = px(position.y);
    drag.propagate(false);
    redraw.write(RequestRedraw);
}

fn close_window(
    surface: SurfaceId,
) -> impl FnMut(On<Pointer<Click>>, ResMut<SurfaceActionQueue>) + Clone {
    move |mut click: On<Pointer<Click>>, mut actions: ResMut<SurfaceActionQueue>| {
        if click.button != PointerButton::Primary {
            return;
        }
        click.propagate(false);
        actions.0.push_back(SurfaceAction::Close { surface });
    }
}

fn random_placement(output_size: Vec2, decorated_size: Vec2, samples: Vec2) -> Vec2 {
    let available = (output_size - decorated_size).max(Vec2::ZERO);
    Vec2::new(
        available.x * samples.x.clamp(0.0, 1.0),
        available.y * samples.y.clamp(0.0, 1.0),
    )
}

fn dragged_position(left: Val, top: Val, delta: Vec2, scale: f32) -> Option<Vec2> {
    let (Val::Px(left), Val::Px(top)) = (left, top) else {
        return None;
    };
    (scale.is_finite() && scale > 0.0).then(|| Vec2::new(left, top) + delta / scale)
}

fn hash_unit(hash: u64) -> f32 {
    (hash as f64 / u64::MAX as f64) as f32
}

fn valid_scale_factor(scale_factor: f64) -> f32 {
    if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor as f32
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bevy::{
        asset::{AssetPlugin, Assets},
        camera::{ManualTextureViewHandle, NormalizedRenderTarget},
        ecs::message::Messages,
        image::Image,
        picking::{
            backend::HitData,
            pointer::{Location, PointerId},
        },
        scene::ScenePlugin,
    };

    use crate::compositor::{
        HostSurfaceEvent, SurfaceCompositorPlugin, SurfaceContentView, SurfaceFrame,
        enqueue_surface_event,
    };

    use super::*;

    fn test_app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((AssetPlugin::default(), ScenePlugin))
            .insert_resource(Assets::<Image>::default())
            .insert_resource(UiScale(1.0))
            .add_message::<RequestRedraw>()
            .add_plugins((
                SurfaceCompositorPlugin,
                DefaultWindowPlugin::new(UVec2::new(1_000, 800), 1.0),
            ));
        let camera = app.world_mut().spawn_empty().id();
        app.insert_resource(CompositorCamera(camera));
        (app, camera)
    }

    fn frame(width: u32, height: u32) -> SurfaceFrame {
        SurfaceFrame {
            width,
            height,
            view: SurfaceContentView {
                source_x: 0.0,
                source_y: 0.0,
                source_width: width as f32,
                source_height: height as f32,
                logical_width: width as f32,
                logical_height: height as f32,
            },
            bgra_pixels: vec![0; width as usize * height as usize * 4],
            opaque: true,
        }
    }

    #[test]
    fn fallback_claims_a_mapped_surface_and_builds_backed_ui_in_one_update() {
        let (mut app, camera) = test_app();
        let surface = SurfaceId::new(37);
        enqueue_surface_event(app.world_mut(), HostSurfaceEvent::Created { surface });
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: frame(320, 240),
            },
        );

        app.update();

        let (source, root) = {
            let mut sources =
                app.world_mut()
                    .query::<(Entity, &AppWindow, &MappedSurface, &PrimaryPresentation)>();
            let (source, window, mapped, presentation) = sources
                .single(app.world())
                .expect("surface should be mapped and claimed");
            assert_eq!(window.surface, surface);
            assert_eq!(mapped.logical_size, Vec2::new(320.0, 240.0));
            (source, presentation.0)
        };
        let root_entity = app
            .world()
            .get_entity(root)
            .expect("presentation root should exist");
        assert_eq!(
            root_entity.get::<PresentsSurface>().map(|claim| claim.0),
            Some(source)
        );
        assert_eq!(
            root_entity.get::<UiTargetCamera>().map(|target| target.0),
            Some(camera),
        );
        assert_eq!(
            root_entity.get::<Node>().map(|node| node.display),
            Some(Display::Flex),
        );

        let mut content_nodes = app
            .world_mut()
            .query::<(&SurfaceNode, &ChildOf, &ImageNode, &Node)>();
        let (surface_node, parent, image, node) = content_nodes
            .single(app.world())
            .expect("presentation should contain one backed surface node");
        assert_eq!(surface_node.surface, surface);
        assert_eq!(parent.parent(), root);
        assert_eq!(node.display, Display::Flex);
        assert_eq!(node.width, px(320.0));
        assert_eq!(node.height, px(240.0));
        assert!(
            app.world()
                .resource::<Assets<Image>>()
                .get(&image.image)
                .is_some()
        );
    }

    #[test]
    fn unmap_preserves_and_hides_the_presentation_then_destroy_cleans_it_up() {
        let (mut app, _) = test_app();
        let surface = SurfaceId::new(41);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: frame(2, 2),
            },
        );
        app.update();
        let (source, root, content) = {
            let mut sources = app.world_mut().query::<(Entity, &PrimaryPresentation)>();
            let (source, presentation) = sources
                .single(app.world())
                .expect("surface should be claimed");
            let root = presentation.0;
            let mut content_nodes = app.world_mut().query::<(Entity, &SurfaceNode)>();
            let (content, _) = content_nodes
                .single(app.world())
                .expect("surface content node should exist");
            (source, root, content)
        };

        enqueue_surface_event(app.world_mut(), HostSurfaceEvent::Unmapped { surface });
        app.update();
        assert!(app.world().get::<MappedSurface>(source).is_none());
        assert_eq!(
            app.world().get::<Node>(root).map(|node| node.display),
            Some(Display::None),
        );
        assert_eq!(
            app.world().get::<Node>(content).map(|node| node.display),
            Some(Display::None),
        );

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: frame(3, 2),
            },
        );
        app.update();
        assert_eq!(
            app.world()
                .get::<PrimaryPresentation>(source)
                .map(|presentation| presentation.0),
            Some(root),
        );
        assert_eq!(
            app.world().get::<Node>(root).map(|node| node.display),
            Some(Display::Flex),
        );
        assert_eq!(
            app.world().get::<Node>(content).map(|node| node.display),
            Some(Display::Flex),
        );

        enqueue_surface_event(app.world_mut(), HostSurfaceEvent::Destroyed { surface });
        app.update();
        assert!(app.world().get_entity(source).is_err());
        assert!(app.world().get_entity(root).is_err());
        assert!(app.world().get_entity(content).is_err());
    }

    #[test]
    fn window_observers_drag_only_chrome_and_emit_close_actions() {
        let (mut app, camera) = test_app();
        app.world_mut().resource_mut::<UiScale>().0 = 1.5;
        let surface = SurfaceId::new(43);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: frame(2, 2),
            },
        );
        app.update();

        let root = {
            let mut roots = app.world_mut().query::<(Entity, &DefaultWindow)>();
            roots
                .single(app.world())
                .expect("default window root should exist")
                .0
        };
        let content = {
            let mut contents = app.world_mut().query::<(Entity, &SurfaceNode)>();
            contents
                .single(app.world())
                .expect("surface content should exist")
                .0
        };
        let (button, header) = {
            let mut buttons = app
                .world_mut()
                .query_filtered::<(Entity, &ChildOf), With<Button>>();
            let (button, parent) = buttons
                .single(app.world())
                .expect("close button should exist");
            (button, parent.parent())
        };
        let initial_position = {
            let node = app
                .world()
                .get::<Node>(root)
                .expect("window root should have layout");
            (node.left, node.top)
        };
        let location = Location {
            target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
            position: Vec2::ZERO,
        };
        let drag = Drag {
            button: PointerButton::Primary,
            distance: Vec2::new(12.0, 8.0),
            delta: Vec2::new(12.0, 8.0),
        };

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location.clone(),
            drag.clone(),
            content,
        ));
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location.clone(),
            drag.clone(),
            button,
        ));
        assert!(app.world().resource::<Messages<RequestRedraw>>().is_empty());
        let node = app
            .world()
            .get::<Node>(root)
            .expect("window root should remain alive");
        assert_eq!((node.left, node.top), initial_position);

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location.clone(),
            drag,
            header,
        ));
        let node = app
            .world()
            .get::<Node>(root)
            .expect("window root should remain alive");
        let Some(position) = dragged_position(
            initial_position.0,
            initial_position.1,
            Vec2::new(12.0, 8.0),
            1.5,
        ) else {
            panic!("random placement should use pixel positions");
        };
        assert_eq!(node.left, px(position.x));
        assert_eq!(node.top, px(position.y));
        assert_eq!(app.world().resource::<Messages<RequestRedraw>>().len(), 1);

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location,
            Click {
                button: PointerButton::Primary,
                hit: HitData::new(camera, 0.0, None, None),
                duration: Duration::ZERO,
                count: 1,
            },
            button,
        ));
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Close { surface }],
        );
    }

    #[test]
    fn random_placement_keeps_the_complete_window_inside_the_output() {
        assert_eq!(
            random_placement(
                Vec2::new(1_000.0, 800.0),
                Vec2::new(640.0, 510.0),
                Vec2::new(1.0, 0.5),
            ),
            Vec2::new(360.0, 145.0),
        );
    }

    #[test]
    fn oversized_windows_start_at_the_output_origin() {
        assert_eq!(
            random_placement(
                Vec2::new(400.0, 300.0),
                Vec2::new(640.0, 510.0),
                Vec2::new(0.5, 0.5),
            ),
            Vec2::ZERO,
        );
    }

    #[test]
    fn drag_screen_pixels_are_converted_to_logical_ui_units() {
        assert_eq!(
            dragged_position(px(20.0), px(30.0), Vec2::new(25.0, 10.0), 1.25),
            Some(Vec2::new(40.0, 38.0)),
        );
    }

    #[test]
    fn output_geometry_updates_size_and_scale_independently() {
        let mut world = World::new();
        world.insert_resource(OutputGeometry::new(UVec2::new(1_000, 800), 1.0));

        set_output_scale_factor(&mut world, 1.25);
        set_output_physical_size(&mut world, UVec2::new(1_250, 1_000));

        assert_eq!(
            world.resource::<OutputGeometry>().logical_size(),
            Vec2::new(1_000.0, 800.0),
        );
    }

    #[test]
    fn surface_actions_are_drained_once() {
        let mut world = World::new();
        world.init_resource::<SurfaceActionQueue>();
        let action = SurfaceAction::Close {
            surface: SurfaceId::new(7),
        };
        world
            .resource_mut::<SurfaceActionQueue>()
            .0
            .push_back(action);

        assert_eq!(take_surface_actions(&mut world), [action]);
        assert!(take_surface_actions(&mut world).is_empty());
    }
}
