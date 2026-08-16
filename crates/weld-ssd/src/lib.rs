//! Opinionated server-side decorations built from Weld window primitives.

use std::collections::HashSet;

const PROFILE_TARGET: &str = "weld_profile";

use bevy::{
    app::{App, Plugin, PreUpdate},
    color::Color,
    ecs::{
        component::Component,
        entity::Entity,
        observer::On,
        query::{With, Without},
        schedule::IntoScheduleConfigs,
        system::{Commands, Query, Res},
        template::template,
    },
    math::Vec2,
    picking::{
        Pickable,
        events::{Click, Pointer},
        pointer::PointerButton,
    },
    prelude::{
        AlignItems, BackgroundColor, BorderColor, BorderRadius, BoxShadow, Button, Children,
        FlexDirection, GlobalZIndex, JustifyContent, Node, Overflow, PositionType, Rot2, Scene,
        SceneList, UiRect, UiTargetCamera, UiTransform, ZIndex, percent, px,
    },
    scene::{CommandsSceneExt, bsn, bsn_list, on},
    window::RequestRedraw,
};
use weld_app::{
    output::{OutputCompositionCamera, PrimaryOutput, WeldOutput},
    surface::{
        ClientToplevel, MappedSurface, ServerDecorated, SurfaceId, SurfaceView, ToplevelResizeEdge,
    },
};
use weld_window::{
    FocusedWindow, PresentationInsets, PresentationOffset, PresentsWindow,
    PrimaryWindowPresentation, WindowGeometryAnchor, WindowIntent, WindowIntentKind,
    WindowOccupant, WindowOutput, WindowOutputIntersections, WindowProjection, WindowSystems,
    WindowZOrder,
};
use weld_window_ui::{WindowMoveHandle, WindowResizeHandle, surface_content_with_node};

const BORDER_WIDTH: f32 = 3.0;
const OUTER_BORDER_RADIUS: f32 = 9.0;
const INNER_BORDER_RADIUS: f32 = OUTER_BORDER_RADIUS - BORDER_WIDTH;
const HEADER_HEIGHT: f32 = 30.0;
const CLOSE_BUTTON_SIZE: f32 = 22.0;
const RESIZE_GRAB_EXTENT: f32 = 12.0;
const RESIZE_HALO_EXTENT: f32 = RESIZE_GRAB_EXTENT - BORDER_WIDTH;
// Absolute children are positioned from the root padding box. Including the
// border in the negative inset makes each handle stop at the body edge.
const RESIZE_HANDLE_INSET: f32 = -(RESIZE_HALO_EXTENT + BORDER_WIDTH);
const FOCUSED_BORDER: Color = Color::srgb(0.35, 0.58, 0.88);
const UNFOCUSED_BORDER: Color = Color::srgb(0.28, 0.34, 0.42);

#[derive(Component, Clone, Copy, Debug)]
struct SsdPresentation;

#[derive(Component, Clone, Copy, Debug, Default)]
struct WindowBody;

/// Installs Weld's validating default server-decoration scene.
pub struct SsdPlugin;

impl Plugin for SsdPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PreUpdate,
            revoke_ssd_presentations.in_set(WindowSystems::PresentationRevoke),
        )
        .add_systems(
            PreUpdate,
            present_ssd_windows.in_set(WindowSystems::PresentationClaim),
        )
        .add_systems(
            PreUpdate,
            reconcile_ssd_projections.in_set(WindowSystems::UiReconcile),
        )
        .add_systems(
            PreUpdate,
            sync_focus_style.in_set(WindowSystems::UiReconcile),
        )
        .add_systems(
            PreUpdate,
            sync_focus_style.in_set(WindowSystems::FinalReconcile),
        );
    }
}

fn revoke_ssd_presentations(
    mut commands: Commands,
    roots: Query<(bevy::ecs::entity::Entity, &WindowProjection), With<SsdPresentation>>,
    windows: Query<&WindowOccupant>,
    occupants: Query<(), With<ServerDecorated>>,
) {
    for (root, projection) in &roots {
        let still_server_decorated = windows
            .get(projection.window())
            .ok()
            .is_some_and(|occupant| occupants.contains(occupant.entity()));
        if !still_server_decorated {
            commands.entity(root).despawn();
        }
    }
}

fn reconcile_ssd_projections(
    mut commands: Commands,
    windows: Query<(
        bevy::ecs::entity::Entity,
        &PrimaryWindowPresentation,
        &WindowOccupant,
        &WindowZOrder,
        &WindowOutputIntersections,
    )>,
    occupants: Query<(
        &ClientToplevel,
        Option<&MappedSurface>,
        Option<&ServerDecorated>,
    )>,
    outputs: Query<&OutputCompositionCamera>,
    roots: Query<(bevy::ecs::entity::Entity, &WindowProjection), With<SsdPresentation>>,
) {
    let mut retained = HashSet::new();
    for (window, primary, _, _, _) in &windows {
        if let Ok((_, projection)) = roots.get(primary.entity()) {
            retained.insert((window, projection.output()));
        }
    }

    let mut secondary_roots = roots
        .iter()
        .filter(
            |(root, projection)| match windows.get(projection.window()) {
                Ok((_, primary, _, _, _)) => *root != primary.entity(),
                Err(_) => true,
            },
        )
        .collect::<Vec<_>>();
    secondary_roots.sort_unstable_by_key(|(root, _)| root.to_bits());
    for (root, projection) in secondary_roots {
        let Ok((_, _, _, _, intersections)) = windows.get(projection.window()) else {
            commands.entity(root).despawn();
            continue;
        };
        if !intersections.contains(projection.output())
            || !retained.insert((projection.window(), projection.output()))
        {
            commands.entity(root).despawn();
        }
    }

    for (window, _, occupant, z_order, intersections) in &windows {
        let Ok((toplevel, Some(_), Some(_))) = occupants.get(occupant.entity()) else {
            continue;
        };
        for output in intersections.iter() {
            if !retained.insert((window, output)) {
                continue;
            }
            let Ok(camera) = outputs.get(output) else {
                continue;
            };
            let Some(camera) = camera.entity() else {
                continue;
            };
            commands
                .spawn_scene(scene(window, toplevel.surface))
                .insert((
                    WindowProjection::new(window, output),
                    UiTargetCamera(camera),
                    SsdPresentation,
                    PresentationOffset::default(),
                    PresentationInsets::new(
                        BORDER_WIDTH,
                        HEADER_HEIGHT + BORDER_WIDTH,
                        BORDER_WIDTH,
                        BORDER_WIDTH,
                    ),
                    WindowGeometryAnchor(Vec2::new(0.0, HEADER_HEIGHT)),
                    GlobalZIndex(z_order.0),
                ));
        }
    }
}

type OutputCameraQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        Option<&'static OutputCompositionCamera>,
        Option<&'static PrimaryOutput>,
    ),
    With<WeldOutput>,
>;

fn present_ssd_windows(
    mut commands: Commands,
    windows: Query<
        (
            bevy::ecs::entity::Entity,
            &WindowOccupant,
            &WindowZOrder,
            Option<&WindowOutput>,
        ),
        Without<PrimaryWindowPresentation>,
    >,
    occupants: Query<(
        &ClientToplevel,
        Option<&MappedSurface>,
        Option<&ServerDecorated>,
    )>,
    outputs: OutputCameraQuery,
) {
    let _presentation_span =
        tracing::trace_span!(target: PROFILE_TARGET, "weld_ssd_present_windows").entered();
    for (window, occupant, z_order, output) in &windows {
        let Ok((toplevel, Some(_), Some(_))) = occupants.get(occupant.entity()) else {
            continue;
        };
        let output = output.map(|output| output.0).or_else(|| {
            outputs
                .iter()
                .find_map(|(output, _, primary)| primary.is_some().then_some(output))
        });
        let Some(output) = output else {
            continue;
        };
        let camera = outputs
            .get(output)
            .ok()
            .and_then(|(_, camera, _)| camera)
            .and_then(OutputCompositionCamera::entity);
        let root = commands
            .spawn_scene(scene(window, toplevel.surface))
            .insert((
                PresentsWindow(window),
                WindowProjection::new(window, output),
                SsdPresentation,
                PresentationOffset::default(),
                PresentationInsets::new(
                    BORDER_WIDTH,
                    HEADER_HEIGHT + BORDER_WIDTH,
                    BORDER_WIDTH,
                    BORDER_WIDTH,
                ),
                WindowGeometryAnchor(Vec2::new(0.0, HEADER_HEIGHT)),
                GlobalZIndex(z_order.0),
            ))
            .id();
        if let Some(camera) = camera {
            commands.entity(root).insert(UiTargetCamera(camera));
        }
    }
}

fn sync_focus_style(
    focus: Res<FocusedWindow>,
    mut roots: Query<(&WindowProjection, &mut BorderColor), With<SsdPresentation>>,
    mut redraw: bevy::ecs::message::MessageWriter<RequestRedraw>,
) {
    let mut changed = false;
    for (projection, mut border) in &mut roots {
        let expected = BorderColor::all(if focus.entity() == Some(projection.window()) {
            FOCUSED_BORDER
        } else {
            UNFOCUSED_BORDER
        });
        if *border != expected {
            *border = expected;
            changed = true;
        }
    }
    if changed {
        redraw.write(RequestRedraw);
    }
}

fn scene(window: bevy::ecs::entity::Entity, surface: SurfaceId) -> impl Scene {
    let content = surface_content_with_node(
        surface,
        SurfaceView::WindowGeometry,
        Node {
            overflow: Overflow::clip(),
            border_radius: BorderRadius::px(0.0, 0.0, INNER_BORDER_RADIUS, INNER_BORDER_RADIUS),
            ..Default::default()
        },
    );
    let resize_handles = resize_handles();
    bsn! {
        Node {
            position_type: PositionType::Absolute,
            flex_direction: FlexDirection::Column,
            border: UiRect::all(px(BORDER_WIDTH)),
            border_radius: BorderRadius::all(px(OUTER_BORDER_RADIUS)),
        }
        BorderColor::all(UNFOCUSED_BORDER)
        template(|_| Ok(window_shadow()))
        Children [
            (
                WindowBody
                Node {
                    flex_direction: FlexDirection::Column,
                    border_radius: BorderRadius::all(px(INNER_BORDER_RADIUS)),
                    overflow: Overflow::clip(),
                }
                BackgroundColor(Color::srgb(0.10, 0.12, 0.16))
                Children [
                    (
                        WindowMoveHandle
                        Node {
                            width: percent(100),
                            height: px(HEADER_HEIGHT),
                            flex_shrink: 0.0,
                            align_items: AlignItems::Center,
                            justify_content: JustifyContent::FlexEnd,
                            border_radius: BorderRadius::px(
                                INNER_BORDER_RADIUS,
                                INNER_BORDER_RADIUS,
                                0.0,
                                0.0,
                            ),
                        }
                        BackgroundColor(Color::srgb(0.14, 0.17, 0.22))
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
                            on(close_window(window))
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
                    {content},
                ]
            ),
            {resize_handles},
        ]
    }
}

fn resize_handles() -> impl SceneList {
    bsn_list![
        resize_handle(
            ToplevelResizeEdge::Top,
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                right: px(0),
                top: px(RESIZE_HANDLE_INSET),
                height: px(RESIZE_GRAB_EXTENT),
                ..Default::default()
            }
        ),
        resize_handle(
            ToplevelResizeEdge::Bottom,
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                right: px(0),
                bottom: px(RESIZE_HANDLE_INSET),
                height: px(RESIZE_GRAB_EXTENT),
                ..Default::default()
            }
        ),
        resize_handle(
            ToplevelResizeEdge::Left,
            Node {
                position_type: PositionType::Absolute,
                left: px(RESIZE_HANDLE_INSET),
                top: px(0),
                bottom: px(0),
                width: px(RESIZE_GRAB_EXTENT),
                ..Default::default()
            }
        ),
        resize_handle(
            ToplevelResizeEdge::Right,
            Node {
                position_type: PositionType::Absolute,
                right: px(RESIZE_HANDLE_INSET),
                top: px(0),
                bottom: px(0),
                width: px(RESIZE_GRAB_EXTENT),
                ..Default::default()
            }
        ),
        resize_handle(
            ToplevelResizeEdge::TopLeft,
            Node {
                position_type: PositionType::Absolute,
                left: px(RESIZE_HANDLE_INSET),
                top: px(RESIZE_HANDLE_INSET),
                width: px(RESIZE_GRAB_EXTENT),
                height: px(RESIZE_GRAB_EXTENT),
                ..Default::default()
            }
        ),
        resize_handle(
            ToplevelResizeEdge::TopRight,
            Node {
                position_type: PositionType::Absolute,
                right: px(RESIZE_HANDLE_INSET),
                top: px(RESIZE_HANDLE_INSET),
                width: px(RESIZE_GRAB_EXTENT),
                height: px(RESIZE_GRAB_EXTENT),
                ..Default::default()
            }
        ),
        resize_handle(
            ToplevelResizeEdge::BottomLeft,
            Node {
                position_type: PositionType::Absolute,
                left: px(RESIZE_HANDLE_INSET),
                bottom: px(RESIZE_HANDLE_INSET),
                width: px(RESIZE_GRAB_EXTENT),
                height: px(RESIZE_GRAB_EXTENT),
                ..Default::default()
            }
        ),
        resize_handle(
            ToplevelResizeEdge::BottomRight,
            Node {
                position_type: PositionType::Absolute,
                right: px(RESIZE_HANDLE_INSET),
                bottom: px(RESIZE_HANDLE_INSET),
                width: px(RESIZE_GRAB_EXTENT),
                height: px(RESIZE_GRAB_EXTENT),
                ..Default::default()
            }
        ),
    ]
}

fn resize_handle(edge: ToplevelResizeEdge, node: Node) -> impl Scene {
    bsn! {
        template(move |_| Ok(WindowResizeHandle(edge)))
        ZIndex(0)
        template(move |_| Ok(node.clone()))
    }
}

fn close_window(
    window: bevy::ecs::entity::Entity,
) -> impl FnMut(On<Pointer<Click>>, Commands) + Clone {
    move |mut click: On<Pointer<Click>>, mut commands: Commands| {
        if click.button != PointerButton::Primary {
            return;
        }
        click.propagate(false);
        commands.trigger(WindowIntent {
            window,
            kind: WindowIntentKind::CloseRequested,
        });
    }
}

fn window_shadow() -> BoxShadow {
    BoxShadow::new(
        Color::srgba(0.0, 0.0, 0.0, 0.55),
        px(0),
        px(12),
        px(2),
        px(24),
    )
}

#[cfg(test)]
mod tests {
    use bevy::{
        app::App,
        asset::{AssetApp, AssetPlugin, Assets},
        camera::{ManualTextureViewHandle, NormalizedRenderTarget},
        image::Image,
        math::{UVec2, Vec2},
        picking::{
            backend::HitData,
            events::{Click, Drag, Pointer, Press},
            pointer::{Location, PointerId},
        },
        scene::ScenePlugin,
        ui::{Display, UiScale, Val, widget::Button},
        window::RequestRedraw,
    };
    use weld_app::{
        output::{OutputGeometry, OutputId, OutputPosition, PrimaryOutput, WeldOutput},
        surface::{
            ClientPopup, ClientToplevel, HostSurfaceEvent, HostSurfaceEventKind, SurfaceAction,
            SurfaceBufferUpdate, SurfaceContentView, SurfaceId, SurfaceLayerId,
            SurfaceLayerPlacement, SurfaceNode, SurfacePlugin, SurfaceTreeSnapshot,
            SurfaceWindowGeometry, ToplevelInteractionRequestKind, ToplevelResizeEdge,
            WindowDecoration, enqueue_surface_event, take_surface_actions,
        },
    };
    use weld_float::FloatPlugin;
    use weld_window::{
        FocusedWindow, OccupiesWindow, PresentationInsets, PresentationOffset,
        PrimaryWindowPresentation, WindowGeometry, WindowGeometryAnchor, WindowInteractionKind,
        WindowInteractionSession, WindowPlugin, WindowVisibility, WindowZOrder,
    };
    use weld_window_ui::{
        PrimarySurfacePresentation, WindowMoveHandle, WindowResizeHandle, WindowUiPlugin,
    };

    use super::*;

    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins((
            bevy::app::TaskPoolPlugin::default(),
            AssetPlugin::default(),
            ScenePlugin,
        ));
        app.init_asset::<bevy::shader::Shader>()
            .insert_resource(Assets::<Image>::default())
            .insert_resource(UiScale(1.0))
            .add_message::<RequestRedraw>()
            .add_plugins((
                SurfacePlugin,
                WindowPlugin,
                WindowUiPlugin,
                SsdPlugin,
                FloatPlugin,
            ));
        app.world_mut().spawn((
            WeldOutput {
                id: OutputId::new(1),
            },
            OutputGeometry::from_physical(UVec2::new(1_000, 800), 1.0),
            OutputPosition::default(),
            PrimaryOutput,
        ));
        app
    }

    fn frame(surface: SurfaceId, width: u32, height: u32) -> HostSurfaceEvent {
        frame_with_geometry(
            surface,
            width,
            height,
            Vec2::ZERO,
            UVec2::new(width, height),
        )
    }

    fn frame_with_geometry(
        surface: SurfaceId,
        width: u32,
        height: u32,
        geometry_origin: Vec2,
        geometry_size: UVec2,
    ) -> HostSurfaceEvent {
        let view = SurfaceContentView {
            source_x: 0.0,
            source_y: 0.0,
            source_width: width as f32,
            source_height: height as f32,
            logical_width: width as f32,
            logical_height: height as f32,
        };
        let geometry_view = SurfaceContentView {
            source_x: geometry_origin.x,
            source_y: geometry_origin.y,
            source_width: geometry_size.x as f32,
            source_height: geometry_size.y as f32,
            logical_width: geometry_size.x as f32,
            logical_height: geometry_size.y as f32,
        };
        HostSurfaceEvent {
            surface,
            kind: HostSurfaceEventKind::TreeSnapshot(SurfaceTreeSnapshot {
                client_mapped: true,
                root: Some(SurfaceLayerPlacement {
                    layer: SurfaceLayerId::new(1),
                    position: Vec2::ZERO,
                    view,
                }),
                window_geometry: Some(SurfaceWindowGeometry {
                    origin: geometry_origin,
                    view: geometry_view,
                }),
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: vec![SurfaceBufferUpdate {
                    layer: SurfaceLayerId::new(1),
                    width,
                    height,
                    content: weld_app::surface::SurfaceBufferContent::Pixels(vec![
                        0;
                        width as usize
                            * height
                                as usize
                            * 4
                    ]),
                    opaque: true,
                }],
            }),
        }
    }

    fn unmapped(surface: SurfaceId) -> HostSurfaceEvent {
        HostSurfaceEvent {
            surface,
            kind: HostSurfaceEventKind::TreeSnapshot(SurfaceTreeSnapshot {
                client_mapped: false,
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: Vec::new(),
            }),
        }
    }

    #[test]
    fn decoration_swap_preserves_content_size_and_close_targets_the_occupant() {
        let mut app = test_app();
        let surface = SurfaceId::new(41);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface,
                kind: HostSurfaceEventKind::Created {
                    decoration: WindowDecoration::ClientSide,
                },
            },
        );
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        app.update();

        let (source, window, client_root) = {
            let mut toplevels =
                app.world_mut()
                    .query::<(bevy::ecs::entity::Entity, &ClientToplevel, &OccupiesWindow)>();
            let (source, _, occupancy) = toplevels
                .single(app.world())
                .expect("mapped toplevel should be admitted");
            let root = app
                .world()
                .get::<PrimaryWindowPresentation>(occupancy.0)
                .expect("client-decorated window should have a presentation")
                .entity();
            (source, occupancy.0, root)
        };
        assert_ne!(source, window);
        assert_eq!(
            app.world()
                .get::<Node>(client_root)
                .expect("presentation should have a UI root")
                .display,
            Display::Flex
        );
        take_surface_actions(app.world_mut());

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface,
                kind: HostSurfaceEventKind::DecorationChanged {
                    decoration: WindowDecoration::ServerSide,
                },
            },
        );
        app.update();

        let ssd_root = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("server-decorated window should have a presentation")
            .entity();
        assert_ne!(client_root, ssd_root);
        assert_eq!(
            app.world().get::<PresentationInsets>(ssd_root).copied(),
            Some(PresentationInsets::new(3.0, 33.0, 3.0, 3.0))
        );
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("managed geometry should survive presentation replacement")
                .size,
            Vec2::new(326.0, 276.0)
        );
        assert!(
            take_surface_actions(app.world_mut())
                .into_iter()
                .all(|action| !matches!(action, SurfaceAction::Resize { .. }))
        );
        assert_eq!(
            app.world_mut()
                .query_filtered::<bevy::ecs::entity::Entity, With<WindowMoveHandle>>()
                .iter(app.world())
                .count(),
            1
        );

        let move_handle = app
            .world_mut()
            .query_filtered::<bevy::ecs::entity::Entity, With<WindowMoveHandle>>()
            .single(app.world())
            .expect("SSD should expose one move handle");
        let close_button = app
            .world_mut()
            .query_filtered::<bevy::ecs::entity::Entity, With<Button>>()
            .single(app.world())
            .expect("SSD should expose one close control");
        let camera = app.world_mut().spawn_empty().id();
        let location = Location {
            target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
            position: Vec2::ZERO,
        };
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location.clone(),
            Press {
                button: PointerButton::Primary,
                hit: HitData::new(camera, 0.0, None, None),
                count: 1,
            },
            close_button,
        ));
        app.update();
        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_none()
        );

        let initial_position = app
            .world()
            .get::<WindowGeometry>(window)
            .expect("managed window should retain geometry")
            .position;
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location.clone(),
            Press {
                button: PointerButton::Primary,
                hit: HitData::new(camera, 0.0, None, None),
                count: 1,
            },
            move_handle,
        ));
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location.clone(),
            Drag {
                button: PointerButton::Primary,
                distance: Vec2::new(12.0, 8.0),
                delta: Vec2::new(12.0, 8.0),
            },
            move_handle,
        ));
        app.update();
        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_some()
        );
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("move intent should update managed geometry")
                .position,
            initial_position + Vec2::new(12.0, 8.0)
        );

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location,
            Click {
                button: PointerButton::Primary,
                hit: HitData::new(camera, 0.0, None, None),
                duration: std::time::Duration::ZERO,
                count: 1,
            },
            close_button,
        ));
        app.update();

        assert!(take_surface_actions(app.world_mut()).contains(&SurfaceAction::Close { surface }));
    }

    #[test]
    fn ssd_content_clips_and_outward_handle_starts_resize() {
        let mut app = test_app();
        let surface = SurfaceId::new(49);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface,
                kind: HostSurfaceEventKind::Created {
                    decoration: WindowDecoration::ServerSide,
                },
            },
        );
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        app.update();

        let window = app
            .world_mut()
            .query::<(&ClientToplevel, &OccupiesWindow)>()
            .single(app.world())
            .expect("server-decorated toplevel should be admitted")
            .1
            .0;
        let (_, content_node) = app
            .world_mut()
            .query::<(&SurfaceNode, &Node)>()
            .single(app.world())
            .expect("SSD should mount one client surface node");
        assert_eq!(content_node.overflow, Overflow::clip());
        assert_eq!(
            content_node.border_radius,
            BorderRadius::px(0.0, 0.0, INNER_BORDER_RADIUS, INNER_BORDER_RADIUS,)
        );

        let outer_size = app
            .world()
            .get::<WindowGeometry>(window)
            .expect("floating manager should initialize outer geometry")
            .size;
        let resize_handle = app
            .world_mut()
            .query::<(bevy::ecs::entity::Entity, &WindowResizeHandle)>()
            .iter(app.world())
            .find_map(|(entity, handle)| (handle.0 == ToplevelResizeEdge::Right).then_some(entity))
            .expect("SSD should expose a right-edge resize handle");
        let camera = app.world_mut().spawn_empty().id();
        let location = Location {
            target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
            position: Vec2::ZERO,
        };
        take_surface_actions(app.world_mut());

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location.clone(),
            Press {
                button: PointerButton::Primary,
                hit: HitData::new(camera, 0.0, Some(Vec2::new(0.495, 0.0).extend(0.0)), None),
                count: 1,
            },
            resize_handle,
        ));
        app.update();
        assert!(matches!(
            app.world().get::<WindowInteractionSession>(window),
            Some(WindowInteractionSession {
                kind: weld_window::WindowInteractionKind::Resize(ToplevelResizeEdge::Right),
                ..
            })
        ));

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location,
            Drag {
                button: PointerButton::Primary,
                distance: Vec2::new(20.0, 0.0),
                delta: Vec2::new(20.0, 0.0),
            },
            resize_handle,
        ));
        app.update();

        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("border resize should update desired outer geometry")
                .size,
            outer_size + Vec2::new(20.0, 0.0)
        );
        assert!(
            take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
                surface,
                logical_size: UVec2::new(340, 240),
            })
        );
    }

    #[test]
    fn client_resize_updates_desired_geometry_and_preserves_the_left_anchor() {
        let mut app = test_app();
        let surface = SurfaceId::new(42);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface,
                kind: HostSurfaceEventKind::Created {
                    decoration: WindowDecoration::ClientSide,
                },
            },
        );
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        app.update();
        let (window, content) = {
            let mut toplevels = app
                .world_mut()
                .query::<(&ClientToplevel, &OccupiesWindow)>();
            let (_, occupancy) = toplevels
                .single(app.world())
                .expect("mapped toplevel should be admitted");
            let window = occupancy.0;
            let content = app
                .world_mut()
                .query_filtered::<bevy::ecs::entity::Entity, With<SurfaceNode>>()
                .single(app.world())
                .expect("client presentation should mount its surface");
            (window, content)
        };
        let initial = *app
            .world()
            .get::<WindowGeometry>(window)
            .expect("floating manager should initialize geometry");
        take_surface_actions(app.world_mut());

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface,
                kind: HostSurfaceEventKind::WindowInteraction(
                    ToplevelInteractionRequestKind::Resize {
                        edges: ToplevelResizeEdge::Left,
                    },
                ),
            },
        );
        app.update();
        assert!(matches!(
            app.world().get::<WindowInteractionSession>(window),
            Some(WindowInteractionSession {
                kind: weld_window::WindowInteractionKind::Resize(ToplevelResizeEdge::Left),
                ..
            })
        ));

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
                position: Vec2::ZERO,
            },
            Drag {
                button: PointerButton::Primary,
                distance: Vec2::new(10.0, 0.0),
                delta: Vec2::new(10.0, 0.0),
            },
            content,
        ));
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
                position: Vec2::ZERO,
            },
            Drag {
                button: PointerButton::Primary,
                distance: Vec2::new(20.0, 0.0),
                delta: Vec2::new(10.0, 0.0),
            },
            content,
        ));
        app.update();

        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("resize intent should update desired geometry")
                .size,
            Vec2::new(300.0, 240.0)
        );
        let resize_actions = take_surface_actions(app.world_mut())
            .into_iter()
            .filter(|action| matches!(action, SurfaceAction::Resize { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            resize_actions,
            vec![SurfaceAction::Resize {
                surface,
                logical_size: UVec2::new(300, 240),
            }]
        );

        enqueue_surface_event(app.world_mut(), frame(surface, 300, 240));
        app.update();
        let anchored_position = app
            .world()
            .get::<WindowGeometry>(window)
            .expect("committed size should preserve the fixed edge")
            .position;
        assert!(
            anchored_position.distance(initial.position + Vec2::new(20.0, 0.0)) < 0.001,
            "committed left resize should keep the opposite edge fixed"
        );

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface,
                kind: HostSurfaceEventKind::WindowInteraction(ToplevelInteractionRequestKind::End),
            },
        );
        app.update();
        enqueue_surface_event(app.world_mut(), frame(surface, 300, 240));
        app.update();
        assert!(
            app.world()
                .get::<WindowInteractionSession>(window)
                .is_none()
        );
    }

    #[test]
    fn popup_reparents_when_the_owner_changes_presentation() {
        let mut app = test_app();
        let owner = SurfaceId::new(43);
        let popup = SurfaceId::new(44);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface: owner,
                kind: HostSurfaceEventKind::Created {
                    decoration: WindowDecoration::ClientSide,
                },
            },
        );
        enqueue_surface_event(
            app.world_mut(),
            frame_with_geometry(owner, 360, 276, Vec2::new(20.0, 18.0), UVec2::new(320, 240)),
        );
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface: popup,
                kind: HostSurfaceEventKind::PopupConfigured(ClientPopup {
                    owner,
                    position: Vec2::new(102.0, 52.0),
                    stack_index: 1,
                }),
            },
        );
        enqueue_surface_event(
            app.world_mut(),
            frame_with_geometry(popup, 140, 100, Vec2::new(10.0, 8.0), UVec2::new(120, 80)),
        );
        app.update();

        let (window, first_window_root) = {
            let mut toplevels = app
                .world_mut()
                .query::<(&ClientToplevel, &OccupiesWindow)>();
            let (_, occupancy) = toplevels
                .single(app.world())
                .expect("owner should be admitted");
            let root = app
                .world()
                .get::<PrimaryWindowPresentation>(occupancy.0)
                .expect("owner should be presented")
                .entity();
            (occupancy.0, root)
        };
        let first_popup_root = app
            .world_mut()
            .query::<(&ClientPopup, &PrimarySurfacePresentation)>()
            .single(app.world())
            .expect("popup should be presented")
            .1
            .entity();
        assert_eq!(
            app.world().get::<PresentationOffset>(first_window_root),
            Some(&PresentationOffset(Vec2::new(-20.0, -18.0)))
        );
        assert_eq!(
            app.world().get::<WindowGeometryAnchor>(first_window_root),
            Some(&WindowGeometryAnchor(Vec2::new(20.0, 18.0)))
        );
        assert!(app.world().get::<BoxShadow>(first_window_root).is_none());
        let first_popup_node = app
            .world()
            .get::<Node>(first_popup_root)
            .expect("popup presentation should have layout");
        assert_eq!(
            (first_popup_node.left, first_popup_node.top),
            (px(112.0), px(62.0))
        );
        assert_eq!(
            app.world()
                .get::<bevy::ecs::hierarchy::ChildOf>(first_popup_root)
                .map(bevy::ecs::hierarchy::ChildOf::parent),
            Some(first_window_root)
        );

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface: owner,
                kind: HostSurfaceEventKind::DecorationChanged {
                    decoration: WindowDecoration::ServerSide,
                },
            },
        );
        app.update();

        let second_window_root = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("owner should receive the replacement presentation")
            .entity();
        let second_popup_root = app
            .world_mut()
            .query::<(&ClientPopup, &PrimarySurfacePresentation)>()
            .single(app.world())
            .expect("popup should be reclaimed after the swap")
            .1
            .entity();
        assert_ne!(first_window_root, second_window_root);
        assert_ne!(first_popup_root, second_popup_root);
        assert_eq!(
            app.world()
                .get::<bevy::ecs::hierarchy::ChildOf>(second_popup_root)
                .map(bevy::ecs::hierarchy::ChildOf::parent),
            Some(second_window_root)
        );
        let second_popup_node = app
            .world()
            .get::<Node>(second_popup_root)
            .expect("replacement popup should have layout");
        assert_eq!(
            (second_popup_node.left, second_popup_node.top),
            (px(92.0), px(74.0))
        );

        take_surface_actions(app.world_mut());
        app.world_mut()
            .entity_mut(window)
            .insert(WindowVisibility::Hidden);
        app.update();
        assert_eq!(
            app.world()
                .get::<Node>(second_window_root)
                .expect("hidden window presentation should remain queryable")
                .display,
            Display::None
        );
        assert_eq!(
            app.world()
                .get::<Node>(second_popup_root)
                .expect("popup should hide with its owner")
                .display,
            Display::None
        );
        assert!(
            take_surface_actions(app.world_mut()).contains(&SurfaceAction::Focus { surface: None })
        );
    }

    #[test]
    fn committed_client_extent_changes_without_overwriting_desired_geometry() {
        let mut app = test_app();
        let surface = SurfaceId::new(45);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface,
                kind: HostSurfaceEventKind::Created {
                    decoration: WindowDecoration::ClientSide,
                },
            },
        );
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        app.update();
        let window = app
            .world_mut()
            .query::<(&ClientToplevel, &OccupiesWindow)>()
            .single(app.world())
            .expect("mapped toplevel should be admitted")
            .1
            .0;
        let root = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("client window should have a presentation")
            .entity();
        assert_eq!(
            app.world()
                .get::<Node>(root)
                .expect("presentation should have layout")
                .width,
            Val::Auto
        );

        enqueue_surface_event(app.world_mut(), frame(surface, 400, 280));
        app.update();

        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("desired geometry should remain manager-authored")
                .size,
            Vec2::new(320.0, 240.0)
        );
        let surface_node = app
            .world_mut()
            .query::<(&SurfaceNode, &Node)>()
            .single(app.world())
            .expect("presentation should retain its content node");
        assert_eq!(
            (surface_node.1.width, surface_node.1.height),
            (px(400.0), px(280.0))
        );
    }

    #[test]
    fn unmap_hides_a_window_and_surface_destruction_removes_the_default_frame() {
        let mut app = test_app();
        let surface = SurfaceId::new(46);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface,
                kind: HostSurfaceEventKind::Created {
                    decoration: WindowDecoration::ClientSide,
                },
            },
        );
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        app.update();
        let window = app
            .world_mut()
            .query::<(&ClientToplevel, &OccupiesWindow)>()
            .single(app.world())
            .expect("mapped toplevel should be admitted")
            .1
            .0;
        let root = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("client window should have a presentation")
            .entity();

        enqueue_surface_event(app.world_mut(), unmapped(surface));
        app.update();
        assert_eq!(
            app.world()
                .get::<Node>(root)
                .expect("unmapped presentation should remain available")
                .display,
            Display::None
        );

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface,
                kind: HostSurfaceEventKind::Destroyed,
            },
        );
        app.update();
        assert!(app.world().get_entity(window).is_err());
        assert!(app.world().get_entity(root).is_err());
    }

    #[test]
    fn multiple_windows_keep_independent_roots_and_focus_falls_back_on_destroy() {
        let mut app = test_app();
        let first = SurfaceId::new(47);
        let second = SurfaceId::new(48);
        for surface in [first, second] {
            enqueue_surface_event(
                app.world_mut(),
                HostSurfaceEvent {
                    surface,
                    kind: HostSurfaceEventKind::Created {
                        decoration: WindowDecoration::ClientSide,
                    },
                },
            );
            enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        }
        app.update();

        let windows = app
            .world_mut()
            .query::<(&ClientToplevel, &OccupiesWindow)>()
            .iter(app.world())
            .map(|(toplevel, occupancy)| (toplevel.surface, occupancy.0))
            .collect::<std::collections::HashMap<_, _>>();
        let first_window = windows[&first];
        let second_window = windows[&second];
        assert_ne!(
            app.world()
                .get::<PrimaryWindowPresentation>(first_window)
                .map(PrimaryWindowPresentation::entity),
            app.world()
                .get::<PrimaryWindowPresentation>(second_window)
                .map(PrimaryWindowPresentation::entity)
        );
        assert_ne!(
            app.world().get::<WindowZOrder>(first_window),
            app.world().get::<WindowZOrder>(second_window)
        );
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(second_window)
        );
        take_surface_actions(app.world_mut());

        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface: second,
                kind: HostSurfaceEventKind::Destroyed,
            },
        );
        app.update();

        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(first_window)
        );
        assert!(
            take_surface_actions(app.world_mut()).contains(&SurfaceAction::Focus {
                surface: Some(first),
            })
        );
    }

    #[test]
    fn rehoming_keeps_one_ssd_projection_per_output() {
        let mut app = test_app();
        let surface = SurfaceId::new(91);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface,
                kind: HostSurfaceEventKind::Created {
                    decoration: WindowDecoration::ServerSide,
                },
            },
        );
        enqueue_surface_event(app.world_mut(), frame(surface, 300, 60));
        app.update();

        let window = app
            .world_mut()
            .query::<(&ClientToplevel, &OccupiesWindow)>()
            .single(app.world())
            .expect("mapped surface should occupy a window")
            .1
            .0;
        let primary_root = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("SSD should claim the window")
            .entity();
        let primary_output = app
            .world()
            .get::<WindowProjection>(primary_root)
            .expect("primary presentation should target an output")
            .output();
        let external = app
            .world_mut()
            .spawn((
                WeldOutput {
                    id: OutputId::new(2),
                },
                OutputGeometry::from_physical(UVec2::new(1_000, 800), 1.0),
                OutputPosition(Vec2::new(0.0, -800.0)),
            ))
            .id();
        app.world_mut().entity_mut(window).insert((
            WindowOutput(external),
            WindowInteractionSession {
                kind: WindowInteractionKind::Move,
            },
        ));
        let mut geometry = app
            .world_mut()
            .get_mut::<WindowGeometry>(window)
            .expect("window should have geometry");
        geometry.position = Vec2::new(100.0, 750.0);
        geometry.size = Vec2::new(300.0, 60.0);
        let secondary_root = app
            .world_mut()
            .spawn((SsdPresentation, WindowProjection::new(window, external)))
            .id();

        app.update();

        let external_roots = app
            .world_mut()
            .query::<(
                bevy::ecs::entity::Entity,
                &WindowProjection,
                &SsdPresentation,
            )>()
            .iter(app.world())
            .filter(|(_, projection, _)| {
                projection.window() == window && projection.output() == external
            })
            .map(|(root, _, _)| root)
            .collect::<Vec<_>>();
        assert_eq!(external_roots, [secondary_root]);
        assert_eq!(
            app.world()
                .get::<WindowProjection>(primary_root)
                .map(|projection| projection.output()),
            Some(primary_output)
        );
    }
}
