//! Default application-window presentation built from project-owned ECS state.

use std::{
    collections::{HashSet, hash_map::RandomState},
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
        system::{Commands, Query, Res, ResMut, SystemParam},
        world::World,
    },
    math::{Rot2, UVec2, Vec2},
    picking::{
        Pickable, PickingSystems,
        events::{Click, Drag, Pointer, Press},
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

use crate::{
    composition::{CompositorCamera, composition_advance_requested},
    layer::{WINDOW_Z_INDEX_MAX, WINDOW_Z_INDEX_MIN},
    surface::{
        AppWindow, MappedSurface, SurfaceAction, SurfaceActionQueue, SurfaceId, SurfaceNode,
        SurfaceSystems,
    },
};

const BORDER_WIDTH: f32 = 3.0;
const HEADER_HEIGHT: f32 = 30.0;
const CLOSE_BUTTON_SIZE: f32 = 22.0;
const FOCUSED_BORDER: Color = Color::srgb(0.35, 0.58, 0.88);
const UNFOCUSED_BORDER: Color = Color::srgb(0.28, 0.34, 0.42);

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

#[derive(Resource, Default)]
struct WindowFocus(Option<SurfaceId>);

#[derive(Resource)]
struct WindowStack {
    next: Option<i32>,
}

impl Default for WindowStack {
    fn default() -> Self {
        Self {
            next: Some(WINDOW_Z_INDEX_MIN),
        }
    }
}

impl WindowStack {
    fn allocate(&mut self) -> Option<i32> {
        let current = self.next?;
        self.next = current
            .checked_add(1)
            .filter(|next| *next <= WINDOW_Z_INDEX_MAX);
        Some(current)
    }
}

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
            .init_resource::<WindowFocus>()
            .init_resource::<WindowStack>()
            .add_systems(
                PreUpdate,
                present_default_windows
                    .run_if(composition_advance_requested)
                    .in_set(SurfaceSystems::FallbackPresentation),
            )
            .add_systems(
                PreUpdate,
                sync_default_window_state
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

#[derive(SystemParam)]
struct PresentWindowParams<'w, 's> {
    commands: Commands<'w, 's>,
    camera: Option<Res<'w, CompositorCamera>>,
    output: Res<'w, OutputGeometry>,
    random: Res<'w, PlacementRandom>,
    focus: ResMut<'w, WindowFocus>,
    stack: ResMut<'w, WindowStack>,
    actions: ResMut<'w, SurfaceActionQueue>,
    surfaces: Query<
        'w,
        's,
        (Entity, &'static AppWindow, &'static MappedSurface),
        Without<PrimaryPresentation>,
    >,
    existing_windows: Query<'w, 's, (Entity, &'static mut GlobalZIndex), With<DefaultWindow>>,
}

fn present_default_windows(mut params: PresentWindowParams) {
    let Some(camera) = params.camera else {
        if !params.surfaces.is_empty() {
            warn!("left mapped surfaces unclaimed because the compositor camera is unavailable");
        }
        return;
    };

    let mut unclaimed = params.surfaces.iter().collect::<Vec<_>>();
    unclaimed.sort_unstable_by_key(|(_, window, _)| window.surface.raw());
    for (source, window, mapped) in unclaimed {
        let decorated_size =
            mapped.logical_size + Vec2::new(BORDER_WIDTH * 2.0, HEADER_HEIGHT + BORDER_WIDTH * 2.0);
        let position = random_placement(
            params.output.logical_size(),
            decorated_size,
            params.random.samples(window.surface),
        );
        let z_index = next_window_z(&mut params.stack, &mut params.existing_windows);
        let mut root = params
            .commands
            .spawn_scene(default_window_scene(window.surface, position));
        let root_entity = root.id();
        root.insert((
            PresentsSurface(source),
            DefaultWindow {
                surface: window.surface,
            },
            UiTargetCamera(camera.0),
            GlobalZIndex(z_index),
        ));
        params
            .commands
            .spawn((
                SurfaceNode {
                    surface: window.surface,
                },
                ImageNode::default(),
                Node {
                    display: Display::None,
                    ..Default::default()
                },
                ChildOf(root_entity),
            ))
            // Observe owned picking targets directly and stop propagation after handling so
            // focus/raise remains exactly-once regardless of UI traversal details.
            .observe(focus_window(window.surface));
        params.focus.0 = Some(window.surface);
        params.actions.push(SurfaceAction::Focus {
            surface: Some(window.surface),
        });
    }
}

fn sync_default_window_state(
    surfaces: Query<(&AppWindow, Option<&MappedSurface>)>,
    mut windows: Query<(&DefaultWindow, &GlobalZIndex, &mut Node, &mut BorderColor)>,
    mut focus: ResMut<WindowFocus>,
    mut actions: ResMut<SurfaceActionQueue>,
) {
    let mapped_surfaces = surfaces
        .iter()
        .filter_map(|(surface, mapped)| mapped.map(|_| surface.surface))
        .collect::<HashSet<_>>();
    for (window, _, mut node, _) in &mut windows {
        let mapped = mapped_surfaces.contains(&window.surface);
        let display = if mapped { Display::Flex } else { Display::None };
        if node.display != display {
            node.display = display;
        }
    }

    let next = windows
        .iter()
        .filter(|(window, _, _, _)| mapped_surfaces.contains(&window.surface))
        .max_by_key(|(_, z_index, _, _)| z_index.0)
        .map(|(window, _, _, _)| window.surface);
    let focused_surface_is_unmapped = focus
        .0
        .is_some_and(|surface| !mapped_surfaces.contains(&surface));
    let visible_surface_needs_focus = focus.0.is_none() && next.is_some();
    if (focused_surface_is_unmapped || visible_surface_needs_focus) && focus.0 != next {
        focus.0 = next;
        actions.push(SurfaceAction::Focus { surface: next });
    }

    for (window, _, _, mut border) in &mut windows {
        let color = if focus.0 == Some(window.surface) {
            FOCUSED_BORDER
        } else {
            UNFOCUSED_BORDER
        };
        if *border != BorderColor::all(color) {
            *border = BorderColor::all(color);
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
        BorderColor::all(UNFOCUSED_BORDER)
        BackgroundColor(Color::srgb(0.10, 0.12, 0.16))
        BoxShadow::new(
            Color::srgba(0.0, 0.0, 0.0, 0.55),
            px(0),
            px(12),
            px(2),
            px(24),
        )
        on(focus_window(surface))
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
                on(focus_window(surface))
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
                    on(focus_window(surface))
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

fn focus_window(surface: SurfaceId) -> impl FnMut(On<Pointer<Press>>, FocusWindowParams) + Clone {
    move |mut press: On<Pointer<Press>>, mut params: FocusWindowParams| {
        if press.button != PointerButton::Primary {
            return;
        }
        let mut window = press.entity;
        while !params.windows.contains(window) {
            let Ok(parent) = params.parents.get(window) else {
                return;
            };
            window = parent.parent();
        }
        press.propagate(false);
        let top_z_index = params.windows.iter().map(|(_, z_index)| z_index.0).max();
        let current_z_index = params
            .windows
            .get(window)
            .ok()
            .map(|(_, z_index)| z_index.0);
        if current_z_index != top_z_index {
            let z_index = next_window_z(&mut params.stack, &mut params.windows);
            if let Ok((_, mut current)) = params.windows.get_mut(window) {
                current.0 = z_index;
            }
        }
        params.focus.0 = Some(surface);
        params.actions.push(SurfaceAction::Focus {
            surface: Some(surface),
        });
        params.redraw.write(RequestRedraw);
    }
}

#[derive(SystemParam)]
struct FocusWindowParams<'w, 's> {
    focus: ResMut<'w, WindowFocus>,
    stack: ResMut<'w, WindowStack>,
    actions: ResMut<'w, SurfaceActionQueue>,
    windows: Query<'w, 's, (Entity, &'static mut GlobalZIndex), With<DefaultWindow>>,
    parents: Query<'w, 's, &'static ChildOf>,
    redraw: MessageWriter<'w, RequestRedraw>,
}

fn next_window_z(
    stack: &mut WindowStack,
    windows: &mut Query<(Entity, &mut GlobalZIndex), With<DefaultWindow>>,
) -> i32 {
    if let Some(z_index) = stack.allocate() {
        return z_index;
    }
    rebase_window_stack(stack, windows);
    stack.allocate().unwrap_or(WINDOW_Z_INDEX_MAX)
}

fn rebase_window_stack(
    stack: &mut WindowStack,
    windows: &mut Query<(Entity, &mut GlobalZIndex), With<DefaultWindow>>,
) {
    let mut order = windows
        .iter_mut()
        .map(|(entity, z_index)| (entity, z_index.0))
        .collect::<Vec<_>>();
    rebase_window_order(stack, &mut order);
    for (entity, z_index) in order {
        if let Ok((_, mut current)) = windows.get_mut(entity) {
            current.0 = z_index;
        }
    }
}

fn rebase_window_order(stack: &mut WindowStack, order: &mut [(Entity, i32)]) {
    order.sort_unstable_by_key(|(entity, z_index)| (*z_index, entity.to_bits()));
    for (offset, (_, z_index)) in order.iter_mut().enumerate() {
        let offset = i32::try_from(offset).unwrap_or(WINDOW_Z_INDEX_MAX);
        *z_index = WINDOW_Z_INDEX_MIN
            .saturating_add(offset)
            .min(WINDOW_Z_INDEX_MAX);
    }
    stack.next = match order.last().map(|(_, z_index)| *z_index) {
        Some(z_index) => z_index
            .checked_add(1)
            .filter(|next| *next <= WINDOW_Z_INDEX_MAX),
        None => Some(WINDOW_Z_INDEX_MIN),
    };
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
        actions.push(SurfaceAction::Close { surface });
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

    use crate::{
        composition::CompositionPlugin,
        layer::SHELL_Z_INDEX,
        surface::{
            HostSurfaceEvent, SurfaceContentView, SurfaceFrame, SurfacePlugin,
            enqueue_surface_event, take_surface_actions,
        },
    };

    use super::*;

    fn test_app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((AssetPlugin::default(), ScenePlugin))
            .insert_resource(Assets::<Image>::default())
            .insert_resource(UiScale(1.0))
            .add_message::<RequestRedraw>()
            .add_plugins((
                CompositionPlugin,
                SurfacePlugin,
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
    fn fallback_presents_multiple_surfaces_with_independent_roots() {
        let (mut app, _) = test_app();
        let first = SurfaceId::new(51);
        let second = SurfaceId::new(52);
        for surface in [first, second] {
            enqueue_surface_event(app.world_mut(), HostSurfaceEvent::Created { surface });
            enqueue_surface_event(
                app.world_mut(),
                HostSurfaceEvent::Frame {
                    surface,
                    frame: frame(4, 3),
                },
            );
        }

        app.update();

        let mut windows = app
            .world_mut()
            .query::<(&DefaultWindow, &GlobalZIndex, &BorderColor)>();
        let mut presented = windows
            .iter(app.world())
            .map(|(window, z_index, border)| (window.surface, z_index.0, *border))
            .collect::<Vec<_>>();
        presented.sort_unstable_by_key(|(surface, _, _)| surface.raw());
        assert_eq!(presented.len(), 2);
        assert_eq!(presented[0].0, first);
        assert_eq!(presented[0].1, WINDOW_Z_INDEX_MIN);
        assert_eq!(presented[0].2, BorderColor::all(UNFOCUSED_BORDER));
        assert_eq!(presented[1].0, second);
        assert_eq!(presented[1].1, WINDOW_Z_INDEX_MIN + 1);
        assert_eq!(presented[1].2, BorderColor::all(FOCUSED_BORDER));
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [
                SurfaceAction::Focus {
                    surface: Some(first),
                },
                SurfaceAction::Focus {
                    surface: Some(second),
                },
            ],
        );
    }

    #[test]
    fn pressing_and_destroying_windows_updates_focus_without_replacing_siblings() {
        let (mut app, camera) = test_app();
        let first = SurfaceId::new(61);
        let second = SurfaceId::new(62);
        for surface in [first, second] {
            enqueue_surface_event(
                app.world_mut(),
                HostSurfaceEvent::Frame {
                    surface,
                    frame: frame(4, 3),
                },
            );
        }
        app.update();
        take_surface_actions(app.world_mut());

        let first_root = {
            let mut windows = app.world_mut().query::<(Entity, &DefaultWindow)>();
            windows
                .iter(app.world())
                .find_map(|(entity, window)| (window.surface == first).then_some(entity))
                .expect("first window should have a presentation root")
        };
        let location = Location {
            target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
            position: Vec2::ZERO,
        };
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location,
            Press {
                button: PointerButton::Primary,
                hit: HitData::new(camera, 0.0, None, None),
                count: 1,
            },
            first_root,
        ));

        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Focus {
                surface: Some(first),
            }],
        );
        let mut windows = app.world_mut().query::<(&DefaultWindow, &GlobalZIndex)>();
        let z_indices = windows
            .iter(app.world())
            .map(|(window, z_index)| (window.surface, z_index.0))
            .collect::<std::collections::HashMap<_, _>>();
        assert!(z_indices[&first] > z_indices[&second]);

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Destroyed { surface: first },
        );
        app.update();

        let mut remaining = app.world_mut().query::<&DefaultWindow>();
        assert_eq!(
            remaining
                .iter(app.world())
                .map(|window| window.surface)
                .collect::<Vec<_>>(),
            [second],
        );
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Focus {
                surface: Some(second),
            }],
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
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Focus {
                surface: Some(surface),
            }],
        );
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
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Focus { surface: None }],
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
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Focus {
                surface: Some(surface),
            }],
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
        assert_eq!(
            take_surface_actions(app.world_mut()),
            [SurfaceAction::Focus {
                surface: Some(surface),
            }],
        );

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
    fn content_and_chrome_presses_reassert_focus_without_burning_stack_slots() {
        let (mut app, camera) = test_app();
        app.world_mut().resource_mut::<UiScale>().0 = 1.5;
        let surface = SurfaceId::new(44);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent::Frame {
                surface,
                frame: frame(2, 2),
            },
        );
        app.update();
        take_surface_actions(app.world_mut());

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
        let next_z_index = app.world().resource::<WindowStack>().next;
        let location = Location {
            target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
            position: Vec2::ZERO,
        };
        let press = Press {
            button: PointerButton::Primary,
            hit: HitData::new(camera, 0.0, None, None),
            count: 1,
        };

        for target in [content, header, button] {
            app.world_mut().trigger(Pointer::new(
                PointerId::Mouse,
                location.clone(),
                press.clone(),
                target,
            ));
            assert_eq!(
                take_surface_actions(app.world_mut()),
                [SurfaceAction::Focus {
                    surface: Some(surface),
                }],
            );
        }
        assert_eq!(app.world().resource::<WindowStack>().next, next_z_index);
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
    fn exhausted_window_stack_rebases_in_order_below_the_shell_band() {
        let mut world = World::new();
        let formerly_top = world.spawn_empty().id();
        let formerly_bottom = world.spawn_empty().id();
        let formerly_middle = world.spawn_empty().id();
        let mut order = [
            (formerly_top, 90),
            (formerly_bottom, 10),
            (formerly_middle, 40),
        ];
        let mut stack = WindowStack { next: None };

        rebase_window_order(&mut stack, &mut order);

        assert_eq!(
            order,
            [
                (formerly_bottom, WINDOW_Z_INDEX_MIN),
                (formerly_middle, WINDOW_Z_INDEX_MIN + 1),
                (formerly_top, WINDOW_Z_INDEX_MIN + 2),
            ],
        );
        assert!(order.iter().all(|(_, z_index)| *z_index < SHELL_Z_INDEX));
        assert_eq!(stack.allocate(), Some(WINDOW_Z_INDEX_MIN + 3));
    }

    #[test]
    fn window_stack_exhausts_at_the_top_of_its_reserved_band() {
        let mut stack = WindowStack {
            next: Some(WINDOW_Z_INDEX_MAX),
        };

        assert_eq!(stack.allocate(), Some(WINDOW_Z_INDEX_MAX));
        assert_eq!(stack.allocate(), None);
    }

    #[test]
    fn surface_actions_are_drained_once() {
        let mut world = World::new();
        world.init_resource::<SurfaceActionQueue>();
        let action = SurfaceAction::Close {
            surface: SurfaceId::new(7),
        };
        world.resource_mut::<SurfaceActionQueue>().push(action);

        assert_eq!(take_surface_actions(&mut world), [action]);
        assert!(take_surface_actions(&mut world).is_empty());
    }
}
