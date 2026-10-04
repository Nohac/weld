//! Opinionated server-side decorations built from Weld window primitives.

use std::collections::HashSet;

mod style;
use style::FrameGeometry;
pub use style::{BorderRequest, BorderStyle, FrameColors, SsdSettings, WindowBorderStyle};

const PROFILE_TARGET: &str = "weld_profile";

use bevy::{
    app::{App, Plugin, PreUpdate},
    color::Color,
    ecs::{
        component::Component,
        entity::Entity,
        observer::On,
        query::{Has, With, Without},
        schedule::IntoScheduleConfigs,
        system::{Commands, Query, Res, SystemParam},
        template::{FromTemplate, template},
    },
    math::Vec2,
    picking::Pickable,
    prelude::{
        AlignItems, BackgroundColor, BorderColor, BorderRadius, BoxShadow, Button, Children,
        Display, FlexDirection, GlobalZIndex, JustifyContent, Node, Overflow, PositionType, Rot2,
        Scene, SceneList, UiRect, UiTargetCamera, UiTransform, ZIndex, percent, px,
    },
    scene::{CommandsSceneExt, bsn, bsn_list},
    window::RequestRedraw,
};
use weld_app::{
    output::{OutputCompositionCamera, PrimaryOutput, WeldOutput},
    surface::{
        ClientProvenance, ClientToplevel, ClientToplevelParent, MappedSurface, ServerDecorated,
        SurfaceId, SurfaceView, ToplevelResizeEdge,
    },
};
use weld_window::{
    FloatingWindow, FocusedWindow, ManagedWindow, PresentationOffset, PresentsWindow,
    PrimaryWindowPresentation, WindowClientResolver, WindowCloseHandle, WindowGeometryAnchor,
    WindowInteractionSession, WindowMoveHandle, WindowOutput, WindowOutputIntersections,
    WindowPresentationOverride, WindowProjection, WindowResizeHandle, WindowSplitEdge,
    WindowSystems, WindowVacancy, WindowZOrder,
};
use weld_window_ui::{server_frame_required, surface_content_with_node};

#[cfg(test)]
const INNER_BORDER_RADIUS: f32 = 6.0;
const HEADER_HEIGHT: f32 = 30.0;
const CLOSE_BUTTON_SIZE: f32 = 22.0;
const RESIZE_GRAB_EXTENT: f32 = 12.0;
const FOCUSED_BORDER: Color = Color::srgb(0.35, 0.58, 0.88);
const UNFOCUSED_BORDER: Color = Color::srgb(0.28, 0.34, 0.42);
const RELOCATED_FOCUSED_BORDER: Color = Color::srgb(0.92, 0.18, 0.16);
const RELOCATED_UNFOCUSED_BORDER: Color = Color::srgb(0.58, 0.12, 0.12);

#[derive(Component, Clone, Copy, Debug)]
struct SsdPresentation;

#[derive(Component, Clone, Copy, Debug)]
struct VacantSsdPresentation;

#[derive(Component, Clone, Copy, Debug, Default)]
struct WindowBody;

#[derive(Component, Clone, Copy, Default)]
struct FrameHeader;

#[derive(Component, Clone, Copy, Default)]
struct FrameForeground;

#[derive(Component, FromTemplate)]
#[relationship(relationship_target = FrameColorNodes)]
struct FrameColorOwner(Entity);

#[derive(Component)]
#[relationship_target(relationship = FrameColorOwner)]
struct FrameColorNodes(Vec<Entity>);

#[derive(SystemParam)]
struct FrameStyles<'w, 's> {
    settings: Res<'w, SsdSettings>,
    windows: StyledWindows<'w, 's>,
}

type StyledWindows<'w, 's> = Query<
    'w,
    's,
    (
        Has<FloatingWindow>,
        Option<&'static WindowBorderStyle>,
        Has<weld_window::fullscreen::WindowFullscreen>,
        Has<weld_window::SoleTiledWindow>,
    ),
    With<ManagedWindow>,
>;

impl FrameStyles<'_, '_> {
    fn requested_border(&self, window: Entity) -> BorderStyle {
        let (floating, style, _, _) = self
            .windows
            .get(window)
            .unwrap_or((false, None, false, false));
        FrameGeometry::new(&self.settings, floating, style.copied()).border
    }
    fn default_border(&self, window: Entity) -> BorderStyle {
        if self
            .windows
            .get(window)
            .is_ok_and(|(floating, _, _, _)| floating)
        {
            self.settings.floating
        } else {
            self.settings.tiled
        }
    }
    fn geometry(&self, window: Entity) -> FrameGeometry {
        let (floating, style, fullscreen, sole_tile) = self
            .windows
            .get(window)
            .unwrap_or((false, None, false, false));
        let mut geometry = FrameGeometry::new(
            &self.settings,
            floating,
            if fullscreen {
                Some(WindowBorderStyle(BorderStyle::None))
            } else {
                style.copied()
            },
        );
        if self.settings.hide_solo_border && sole_tile && !floating && !fullscreen {
            geometry.border = match geometry.border {
                BorderStyle::Normal(_) => BorderStyle::Normal(0),
                BorderStyle::Pixel(_) | BorderStyle::None => BorderStyle::None,
            };
            if geometry.border == BorderStyle::None {
                geometry.radius = 0.0;
            }
        }
        geometry
    }
}

fn border_request(
    event: On<BorderRequest>,
    focus: Res<FocusedWindow>,
    styles: FrameStyles,
    mut commands: Commands,
    mut redraw: bevy::ecs::message::MessageWriter<RequestRedraw>,
) {
    if let Some(window) = focus
        .entity()
        .filter(|window| styles.windows.contains(*window))
    {
        let style = event.0.unwrap_or_else(|| {
            styles
                .requested_border(window)
                .toggle(styles.default_border(window))
        });
        commands.entity(window).insert(WindowBorderStyle(style));
        redraw.write(RequestRedraw);
    }
}

/// Installs Weld's validating default server-decoration scene.
pub struct SsdPlugin;

impl Plugin for SsdPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SsdSettings>()
            .add_observer(border_request)
            .add_systems(
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
            )
            .add_systems(
                PreUpdate,
                request_style_refresh.in_set(WindowSystems::FinalReconcile),
            );
    }
}

fn revoke_ssd_presentations(
    mut commands: Commands,
    roots: Query<
        (
            bevy::ecs::entity::Entity,
            &WindowProjection,
            Option<&VacantSsdPresentation>,
            &FrameGeometry,
        ),
        With<SsdPresentation>,
    >,
    windows: Query<(&WindowVacancy, Option<&WindowPresentationOverride>)>,
    clients: WindowClientResolver,
    occupants: Query<(Option<&MappedSurface>, Has<ServerDecorated>), With<ClientToplevel>>,
    styles: FrameStyles,
) {
    for (root, projection, vacant_presentation, geometry) in &roots {
        let still_server_decorated =
            windows
                .get(projection.window())
                .is_ok_and(|(vacancy, presentation_override)| {
                    presentation_override.is_none()
                        && clients.client_entity(projection.window()).map_or(
                            vacant_presentation.is_some() && *vacancy == WindowVacancy::Retain,
                            |client| {
                                vacant_presentation.is_none()
                                    && occupants.get(client).is_ok_and(|(mapped, requested)| {
                                        // Unmapping hides the frame; remapping resolves policy.
                                        mapped.is_none() || server_frame_required(requested, mapped)
                                    })
                            },
                        )
                });
        if !still_server_decorated || *geometry != styles.geometry(projection.window()) {
            commands.entity(root).despawn();
        }
    }
}

type ProjectedSsdWindows<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static PrimaryWindowPresentation,
        &'static WindowVacancy,
        &'static WindowZOrder,
        &'static WindowOutputIntersections,
        Option<&'static WindowPresentationOverride>,
    ),
>;

fn reconcile_ssd_projections(
    mut commands: Commands,
    windows: ProjectedSsdWindows,
    clients: WindowClientResolver,
    occupants: Query<(
        &ClientToplevel,
        Option<&MappedSurface>,
        Option<&ServerDecorated>,
    )>,
    outputs: Query<&OutputCompositionCamera>,
    roots: Query<(bevy::ecs::entity::Entity, &WindowProjection), With<SsdPresentation>>,
    styles: FrameStyles,
) {
    let mut retained = HashSet::new();
    for (window, primary, _, _, _, presentation_override) in &windows {
        if presentation_override.is_some() {
            continue;
        }
        if let Ok((_, projection)) = roots.get(primary.entity()) {
            retained.insert((window, projection.output()));
        }
    }

    let mut secondary_roots = roots
        .iter()
        .filter(
            |(root, projection)| match windows.get(projection.window()) {
                Ok((_, primary, _, _, _, presentation_override)) => {
                    presentation_override.is_some() || *root != primary.entity()
                }
                Err(_) => true,
            },
        )
        .collect::<Vec<_>>();
    secondary_roots.sort_unstable_by_key(|(root, _)| root.to_bits());
    for (root, projection) in secondary_roots {
        let Ok((_, _, _, _, intersections, presentation_override)) =
            windows.get(projection.window())
        else {
            commands.entity(root).despawn();
            continue;
        };
        if presentation_override.is_some()
            || !intersections.contains(projection.output())
            || !retained.insert((projection.window(), projection.output()))
        {
            commands.entity(root).despawn();
        }
    }

    for (window, _, vacancy, z_order, intersections, presentation_override) in &windows {
        if presentation_override.is_some() {
            continue;
        }
        let content = match clients.client_entity(window) {
            Some(client) => {
                let Ok((toplevel, Some(mapped), requested)) = occupants.get(client) else {
                    continue;
                };
                if !server_frame_required(requested.is_some(), Some(mapped)) {
                    continue;
                }
                SsdContent::Surface(toplevel.surface)
            }
            None if *vacancy == WindowVacancy::Retain => SsdContent::Vacant,
            None => continue,
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
            let style = styles.geometry(window);
            let root = spawn_ssd_scene(&mut commands, content, style);
            let mut root_commands = commands.entity(root);
            root_commands.insert((
                WindowProjection::new(window, output),
                UiTargetCamera(camera),
                SsdPresentation,
                PresentationOffset::default(),
                style.border.insets(),
                WindowGeometryAnchor(Vec2::new(0.0, style.border.header())),
                GlobalZIndex(z_order.0),
            ));
            if content == SsdContent::Vacant {
                root_commands.insert(VacantSsdPresentation);
            }
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

type UnpresentedSsdWindows<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static WindowVacancy,
        &'static WindowZOrder,
        Option<&'static WindowOutput>,
    ),
    (
        Without<PrimaryWindowPresentation>,
        Without<WindowPresentationOverride>,
    ),
>;

fn present_ssd_windows(
    mut commands: Commands,
    windows: UnpresentedSsdWindows,
    clients: WindowClientResolver,
    occupants: Query<(
        &ClientToplevel,
        Option<&MappedSurface>,
        Option<&ServerDecorated>,
    )>,
    outputs: OutputCameraQuery,
    styles: FrameStyles,
) {
    let _presentation_span =
        tracing::trace_span!(target: PROFILE_TARGET, "weld_ssd_present_windows").entered();
    for (window, vacancy, z_order, output) in &windows {
        let content = match clients.client_entity(window) {
            Some(client) => {
                let Ok((toplevel, Some(mapped), requested)) = occupants.get(client) else {
                    continue;
                };
                if !server_frame_required(requested.is_some(), Some(mapped)) {
                    continue;
                }
                SsdContent::Surface(toplevel.surface)
            }
            None if *vacancy == WindowVacancy::Retain => SsdContent::Vacant,
            None => continue,
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
        let style = styles.geometry(window);
        let root = spawn_ssd_scene(&mut commands, content, style);
        commands.entity(root).insert((
            PresentsWindow(window),
            WindowProjection::new(window, output),
            SsdPresentation,
            PresentationOffset::default(),
            style.border.insets(),
            WindowGeometryAnchor(Vec2::new(0.0, style.border.header())),
            GlobalZIndex(z_order.0),
        ));
        if content == SsdContent::Vacant {
            commands.entity(root).insert(VacantSsdPresentation);
        }
        if let Some(camera) = camera {
            commands.entity(root).insert(UiTargetCamera(camera));
        }
    }
}

#[derive(SystemParam)]
struct FocusColors<'w, 's> {
    focus: Res<'w, FocusedWindow>,
    clients: WindowClientResolver<'w, 's>,
    settings: Res<'w, SsdSettings>,
    interactions: Query<'w, 's, (), With<WindowInteractionSession>>,
    splits: Query<'w, 's, &'static WindowSplitEdge, Without<FloatingWindow>>,
    parents: Query<'w, 's, &'static ClientToplevelParent>,
}

impl FocusColors<'_, '_> {
    fn palette(&self, window: Entity) -> FrameColors {
        if self.clients.client_entity(window).is_none() {
            return self.settings.placeholder;
        }
        let focused = self.focus.entity() == Some(window);
        let relocated = self
            .clients
            .mapped_client(window)
            .is_some_and(|client| client.provenance() == ClientProvenance::Relocated);
        match (relocated, focused) {
            (true, true) => self.settings.relocated_focused,
            (true, false) => self.settings.relocated_unfocused,
            (false, true) => self.settings.focused,
            (false, false) => {
                let parent = self
                    .focus
                    .entity()
                    .and_then(|focused| self.clients.client_entity(focused))
                    .and_then(|client| self.parents.get(client).ok());
                if parent.is_some_and(|parent| {
                    self.clients
                        .mapped_client(window)
                        .is_some_and(|client| client.surface() == parent.surface)
                }) {
                    self.settings.focused_inactive
                } else {
                    self.settings.unfocused
                }
            }
        }
    }
}

type FrameStyleRoots<'w, 's> = Query<
    'w,
    's,
    (
        &'static WindowProjection,
        &'static FrameGeometry,
        &'static FrameColorNodes,
        &'static mut BorderColor,
    ),
    With<SsdPresentation>,
>;

fn request_style_refresh(
    styles: FrameStyles,
    roots: Query<(&WindowProjection, &FrameGeometry), With<SsdPresentation>>,
    mut redraw: bevy::ecs::message::MessageWriter<RequestRedraw>,
) {
    if roots
        .iter()
        .any(|(projection, geometry)| *geometry != styles.geometry(projection.window()))
    {
        redraw.write(RequestRedraw);
    }
}

fn sync_focus_style(
    colors: FocusColors,
    mut roots: FrameStyleRoots,
    mut nodes: Query<(&mut BackgroundColor, Has<FrameHeader>)>,
    mut redraw: bevy::ecs::message::MessageWriter<RequestRedraw>,
) {
    let mut changed = false;
    for (projection, geometry, children, mut border) in &mut roots {
        let palette = colors.palette(projection.window());
        let mut expected = if colors.interactions.contains(projection.window()) {
            BorderColor::all(palette.indicator)
        } else if matches!(geometry.border, BorderStyle::Normal(_)) {
            BorderColor {
                top: palette.border,
                left: palette.child_border,
                right: palette.child_border,
                bottom: palette.child_border,
            }
        } else {
            BorderColor::all(palette.child_border)
        };
        if colors.focus.entity() == Some(projection.window())
            && let Ok(edge) = colors.splits.get(projection.window())
        {
            match edge {
                WindowSplitEdge::Left => expected.left = palette.indicator,
                WindowSplitEdge::Bottom => expected.bottom = palette.indicator,
            }
        }
        if *border != expected {
            *border = expected;
            changed = true;
        }
        for child in &children.0 {
            let Ok((mut background, header)) = nodes.get_mut(*child) else {
                continue;
            };
            let expected = if header {
                palette.background
            } else {
                palette.foreground
            };
            if background.0 != expected {
                background.0 = expected;
                changed = true;
            }
        }
    }
    if changed {
        redraw.write(RequestRedraw);
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SsdContent {
    Surface(SurfaceId),
    Vacant,
}

fn spawn_ssd_scene(commands: &mut Commands, content: SsdContent, style: FrameGeometry) -> Entity {
    let root = match content {
        SsdContent::Surface(surface) => commands.spawn_scene(scene(surface, style)).id(),
        SsdContent::Vacant => commands.spawn_scene(vacant_scene(style)).id(),
    };
    commands.entity(root).insert(style);
    root
}

fn scene(surface: SurfaceId, style: FrameGeometry) -> impl Scene {
    let content = surface_content_with_node(
        surface,
        SurfaceView::WindowGeometry,
        Node {
            overflow: Overflow::clip(),
            border_radius: BorderRadius::px(
                if style.border.header() > 0.0 {
                    0.0
                } else {
                    style.inner_radius()
                },
                if style.border.header() > 0.0 {
                    0.0
                } else {
                    style.inner_radius()
                },
                style.inner_radius(),
                style.inner_radius(),
            ),
            ..Default::default()
        },
    );
    window_scene(content, style)
}

fn vacant_scene(style: FrameGeometry) -> impl Scene {
    let content = bsn_list! {
        (
            Node { width: percent(100), height: percent(100) }
            BackgroundColor(Color::srgb(0.10, 0.12, 0.16))
        )
    };
    window_scene(content, style)
}

fn window_scene(content: impl SceneList, style: FrameGeometry) -> impl Scene {
    let resize_handles = resize_handles(style.border);
    bsn! {
        #FrameRoot
        Node {
            position_type: PositionType::Absolute,
            flex_direction: FlexDirection::Column,
            border: UiRect::all(px(style.border.width())),
            border_radius: BorderRadius::all(px(style.radius)),
        }
        BorderColor::all(UNFOCUSED_BORDER)
        template(move |_| Ok(if style.border == BorderStyle::None { BoxShadow::default() } else { window_shadow() }))
        Children [
            (
                WindowBody
                Node {
                    width: percent(100),
                    height: percent(100),
                    min_width: px(0),
                    min_height: px(0),
                    flex_direction: FlexDirection::Column,
                    border_radius: BorderRadius::all(px(style.inner_radius())),
                    overflow: Overflow::clip(),
                }
                BackgroundColor(Color::srgb(0.10, 0.12, 0.16))
                Children [
                    (
                        WindowMoveHandle
                        FrameHeader
                        FrameColorOwner(#FrameRoot)
                        Node {
                            display: {if style.border.header() > 0.0 { Display::Flex } else { Display::None }},
                            width: percent(100),
                            height: px(style.border.header()),
                            flex_shrink: 0.0,
                            align_items: AlignItems::Center,
                            justify_content: JustifyContent::FlexEnd,
                            border_radius: BorderRadius::px(
                                style.inner_radius(),
                                style.inner_radius(),
                                0.0,
                                0.0,
                            ),
                        }
                        BackgroundColor(Color::srgb(0.14, 0.17, 0.22))
                        Children [(
                            Button
                            WindowCloseHandle
                            Node {
                                width: px(CLOSE_BUTTON_SIZE),
                                height: px(CLOSE_BUTTON_SIZE),
                                margin: UiRect::right(px(4)),
                                align_items: AlignItems::Center,
                                justify_content: JustifyContent::Center,
                                border_radius: BorderRadius::MAX,
                            }
                            BackgroundColor(Color::srgb(0.54, 0.16, 0.18))
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
                                        FrameForeground
                                        FrameColorOwner(#FrameRoot)
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
                                        FrameForeground
                                        FrameColorOwner(#FrameRoot)
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

fn resize_handles(border: BorderStyle) -> impl SceneList {
    // Reach from the padding-box edge through the entire visible border.
    let extent = RESIZE_GRAB_EXTENT.max(border.width());
    let inset = -extent;
    let handle = |edge, mut node: Node| {
        if border == BorderStyle::None {
            node.display = Display::None;
        }
        resize_handle(edge, node)
    };
    bsn_list![
        handle(
            ToplevelResizeEdge::Top,
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                right: px(0),
                top: px(inset),
                height: px(extent),
                ..Default::default()
            }
        ),
        handle(
            ToplevelResizeEdge::Bottom,
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                right: px(0),
                bottom: px(inset),
                height: px(extent),
                ..Default::default()
            }
        ),
        handle(
            ToplevelResizeEdge::Left,
            Node {
                position_type: PositionType::Absolute,
                left: px(inset),
                top: px(0),
                bottom: px(0),
                width: px(extent),
                ..Default::default()
            }
        ),
        handle(
            ToplevelResizeEdge::Right,
            Node {
                position_type: PositionType::Absolute,
                right: px(inset),
                top: px(0),
                bottom: px(0),
                width: px(extent),
                ..Default::default()
            }
        ),
        handle(
            ToplevelResizeEdge::TopLeft,
            Node {
                position_type: PositionType::Absolute,
                left: px(inset),
                top: px(inset),
                width: px(extent),
                height: px(extent),
                ..Default::default()
            }
        ),
        handle(
            ToplevelResizeEdge::TopRight,
            Node {
                position_type: PositionType::Absolute,
                right: px(inset),
                top: px(inset),
                width: px(extent),
                height: px(extent),
                ..Default::default()
            }
        ),
        handle(
            ToplevelResizeEdge::BottomLeft,
            Node {
                position_type: PositionType::Absolute,
                left: px(inset),
                bottom: px(inset),
                width: px(extent),
                height: px(extent),
                ..Default::default()
            }
        ),
        handle(
            ToplevelResizeEdge::BottomRight,
            Node {
                position_type: PositionType::Absolute,
                right: px(inset),
                bottom: px(inset),
                width: px(extent),
                height: px(extent),
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
        ecs::system::RunSystemOnce,
        image::Image,
        input::{
            ButtonState,
            mouse::{MouseButton, MouseButtonInput, MouseMotion},
        },
        math::{UVec2, Vec2},
        picking::{
            backend::HitData,
            events::{Click, Pointer, Press},
            pointer::{Location, PointerButton, PointerId},
        },
        scene::ScenePlugin,
        ui::{Display, UiScale, widget::Button},
        window::RequestRedraw,
    };
    use weld_app::{
        output::{OutputGeometry, OutputId, OutputPosition, PrimaryOutput, WeldOutput},
        surface::{
            ClientDecorated, ClientPopup, ClientToplevel, HostSurfaceEvent, HostSurfaceEventKind,
            SurfaceAction, SurfaceAlphaMode, SurfaceBufferContent, SurfaceBufferUpdate,
            SurfaceContentView, SurfaceId, SurfaceLayerId, SurfaceLayerPlacement, SurfaceNode,
            SurfacePlugin, SurfaceTreeSnapshot, SurfaceWindowGeometry,
            ToplevelInteractionRequestKind, ToplevelResizeEdge, WindowDecoration,
            enqueue_surface_event, register_client_source, take_surface_actions,
        },
    };
    use weld_client::{ClientId, ClientSourceDescriptor, ClientSourceId};
    use weld_float::FloatPlugin;
    use weld_window::{
        FocusedWindow, OccupiesWindow, PresentationInsets, PresentationOffset,
        PrimaryWindowPresentation, WindowCommand, WindowCommandKind, WindowGeometry,
        WindowGeometryAnchor, WindowInteractionKind, WindowInteractionSession, WindowMoveHandle,
        WindowPlugin, WindowResizeHandle, WindowVisibility, WindowZOrder,
    };
    use weld_window_ui::{PrimarySurfacePresentation, WindowUiPlugin};

    use super::*;

    fn test_app() -> App {
        let mut app = base_test_app();
        app.add_plugins(FloatPlugin);
        app
    }

    fn base_test_app() -> App {
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
            .add_plugins((SurfacePlugin, WindowPlugin, WindowUiPlugin, SsdPlugin));
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

    #[test]
    fn initial_client_fullscreen_waits_for_admission_and_preserves_dialog_access() {
        use weld_app::surface::{ClientToplevelParent, PendingClientFullscreen};
        use weld_window::fullscreen::{FullscreenOccluded, FullscreenPlugin, WindowFullscreen};
        let mut app = test_app();
        app.add_plugins(FullscreenPlugin);
        let surface = SurfaceId::for_test(109);
        enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ServerSide));
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface,
                kind: HostSurfaceEventKind::StateRequest(
                    weld_client::ToplevelStateRequestKind::Fullscreen(true),
                ),
            },
        );
        app.update();
        assert_eq!(
            app.world_mut()
                .query::<&PendingClientFullscreen>()
                .iter(app.world())
                .count(),
            1
        );
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        app.update();
        assert!(
            take_surface_actions(app.world_mut())
                .iter()
                .all(|action| !matches!(
                    action,
                    SurfaceAction::Resize {
                        fullscreen: false,
                        ..
                    }
                )),
            "admission must not revoke an initial fullscreen hint before policy applies"
        );
        app.update();
        let owner = app
            .world_mut()
            .query::<&OccupiesWindow>()
            .single(app.world())
            .expect("occupancy")
            .0;
        assert!(app.world().get::<WindowFullscreen>(owner).is_some());
        let dialog = SurfaceId::for_test(110);
        let mut dialog_role = role(dialog, WindowDecoration::ServerSide);
        if let HostSurfaceEventKind::Role(weld_client::ClientSurfaceRole::Toplevel(state)) =
            &mut dialog_role.kind
        {
            state.parent = Some(surface);
        }
        enqueue_surface_event(app.world_mut(), dialog_role);
        enqueue_surface_event(app.world_mut(), frame(dialog, 200, 100));
        app.update();
        let dialog_window = app
            .world_mut()
            .query_filtered::<&OccupiesWindow, With<ClientToplevelParent>>()
            .single(app.world())
            .expect("dialog")
            .0;
        assert!(
            app.world()
                .get::<FullscreenOccluded>(dialog_window)
                .is_none()
        );
        app.world_mut().trigger(WindowCommand {
            window: dialog_window,
            kind: WindowCommandKind::Focus,
        });
        app.update();
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(dialog_window)
        );
        app.world_mut().trigger(weld_window::WindowIntent {
            window: owner,
            kind: weld_window::WindowIntentKind::Activate,
        });
        assert_eq!(
            app.world().get::<WindowZOrder>(owner),
            Some(&WindowZOrder(0)),
            "activation must not briefly raise the fullscreen owner over its dialog"
        );
        app.update();
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(owner),
            "clicking the fullscreen floating owner restores its focus"
        );
        enqueue_surface_event(
            app.world_mut(),
            role(SurfaceId::for_test(111), WindowDecoration::ServerSide),
        );
        enqueue_surface_event(app.world_mut(), frame(SurfaceId::for_test(111), 200, 100));
        app.update();
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(owner)
        );
        let hidden = app
            .world_mut()
            .query_filtered::<Entity, With<FullscreenOccluded>>()
            .single(app.world())
            .expect("unrelated hidden window");
        let root = app
            .world()
            .get::<PrimaryWindowPresentation>(hidden)
            .expect("hidden frame")
            .entity();
        assert_eq!(
            app.world().get::<Node>(root).expect("frame node").display,
            Display::None
        );
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface: SurfaceId::for_test(111),
                kind: HostSurfaceEventKind::StateRequest(
                    weld_client::ToplevelStateRequestKind::Fullscreen(true),
                ),
            },
        );
        app.update();
        app.update();
        assert!(
            app.world().get::<WindowFullscreen>(hidden).is_none(),
            "background fullscreen request cannot steal output"
        );
        assert!(app.world().get::<WindowFullscreen>(owner).is_some());
        enqueue_surface_event(app.world_mut(), unmapped(surface));
        app.update();
        assert!(
            app.world().get::<FullscreenOccluded>(hidden).is_none(),
            "unmapped fullscreen owner releases presentation"
        );
        enqueue_surface_event(app.world_mut(), frame(surface, 1000, 800));
        app.update();
        assert!(
            app.world().get::<FullscreenOccluded>(hidden).is_some(),
            "remapping restores the retained claim"
        );
    }

    #[test]
    fn fullscreen_configures_content_and_restores_floating_frame_without_size_drift() {
        use weld_window::fullscreen::{
            FullscreenAction, FullscreenMode, FullscreenPlugin, FullscreenRequest,
        };
        let mut app = test_app();
        app.add_plugins(FullscreenPlugin);
        let surface = SurfaceId::for_test(108);
        enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ServerSide));
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        app.update();
        let window = app
            .world_mut()
            .query::<&OccupiesWindow>()
            .single(app.world())
            .expect("occupancy")
            .0;
        let original = *app.world().get::<WindowGeometry>(window).expect("geometry");
        for _ in 0..3 {
            take_surface_actions(app.world_mut());
            app.world_mut().trigger(FullscreenRequest {
                window: Some(window),
                action: FullscreenAction::Enable(FullscreenMode::Normal),
            });
            app.update();
            let root = app
                .world()
                .get::<PrimaryWindowPresentation>(window)
                .expect("frame")
                .entity();
            assert_eq!(
                app.world().get::<PresentationInsets>(root),
                Some(&PresentationInsets::default())
            );
            assert_eq!(
                app.world()
                    .get::<WindowGeometry>(window)
                    .expect("fullscreen geometry")
                    .size,
                Vec2::new(1000.0, 800.0)
            );
            assert!(
                take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
                    surface,
                    logical_size: UVec2::new(1000, 800),
                    resizing: false,
                    fullscreen: true,
                })
            );
            app.world_mut().trigger(FullscreenRequest {
                window: Some(window),
                action: FullscreenAction::Disable,
            });
            app.update();
            assert_eq!(app.world().get::<WindowGeometry>(window), Some(&original));
            assert!(
                take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
                    surface,
                    logical_size: UVec2::new(320, 240),
                    resizing: false,
                    fullscreen: false,
                })
            );
        }
    }

    fn tiled_test_app() -> App {
        let mut app = base_test_app();
        app.add_plugins(weld_tile::TilePlugin);
        let output = app
            .world_mut()
            .query_filtered::<Entity, With<WeldOutput>>()
            .single(app.world())
            .expect("output");
        app.world_mut()
            .run_system_once(
                move |mut creation: weld_window::workspace::WorkspaceCreation,
                      mut commands: Commands| {
                    let workspace = creation.create("1".into(), output).expect("workspace");
                    commands.trigger(weld_window::workspace::WorkspaceRequest::SetVisible {
                        workspace,
                        visible: true,
                    });
                    commands.trigger(weld_window::workspace::WorkspaceRequest::Focus(workspace));
                },
            )
            .expect("workspace setup");
        app
    }

    #[test]
    fn focused_split_edge_updates_in_place_and_leaves_other_borders_alone() {
        use weld_tile::{SplitAxis, TileOperation, TileRequest};
        let mut app = tiled_test_app();
        {
            let mut settings = app.world_mut().resource_mut::<SsdSettings>();
            settings.tiled = BorderStyle::Pixel(3);
            settings.focused.indicator = Color::WHITE;
        }
        let surface = SurfaceId::for_test(310);
        enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ServerSide));
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        app.update();
        app.update();
        let window = app
            .world_mut()
            .query::<&OccupiesWindow>()
            .single(app.world())
            .expect("window")
            .0;
        let root = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("root")
            .entity();
        let base = app.world().resource::<SsdSettings>().focused.child_border;
        let left = BorderColor {
            left: Color::WHITE,
            ..BorderColor::all(base)
        };
        assert_eq!(app.world().get::<BorderColor>(root), Some(&left));
        app.world_mut()
            .trigger(TileRequest::Focused(TileOperation::Split(
                SplitAxis::Vertical,
            )));
        app.update();
        let bottom = BorderColor {
            bottom: Color::WHITE,
            ..BorderColor::all(base)
        };
        assert_eq!(
            app.world()
                .get::<PrimaryWindowPresentation>(window)
                .expect("same root")
                .entity(),
            root
        );
        assert_eq!(app.world().get::<BorderColor>(root), Some(&bottom));
        app.world_mut()
            .resource_mut::<SsdSettings>()
            .focused
            .indicator = Color::BLACK;
        app.update();
        assert_eq!(
            app.world().get::<BorderColor>(root),
            Some(&BorderColor {
                bottom: Color::BLACK,
                ..bottom
            })
        );
        app.world_mut().trigger(WindowCommand {
            window,
            kind: WindowCommandKind::ClearFocus,
        });
        // Run only the style system: the tiler normally restores selection for a
        // workspace containing one window before the next presentation.
        app.world_mut()
            .run_system_once(sync_focus_style)
            .expect("unfocused style");
        let unfocused = app.world().resource::<SsdSettings>().unfocused.child_border;
        assert_eq!(
            app.world().get::<BorderColor>(root),
            Some(&BorderColor::all(unfocused))
        );
        app.world_mut().trigger(WindowCommand {
            window,
            kind: WindowCommandKind::Focus,
        });
        app.world_mut()
            .trigger(BorderRequest(Some(BorderStyle::None)));
        app.update();
        let root = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("borderless root")
            .entity();
        assert_eq!(
            app.world().get::<PresentationInsets>(root),
            Some(&PresentationInsets::default())
        );
    }

    #[test]
    fn solo_border_toggle_preserves_requested_width_with_gaps_and_retained_slots() {
        let mut app = tiled_test_app();
        {
            let mut settings = app.world_mut().resource_mut::<SsdSettings>();
            settings.tiled = BorderStyle::Pixel(7);
            settings.hide_solo_border = true;
        }
        let first_surface = SurfaceId::for_test(303);
        enqueue_surface_event(
            app.world_mut(),
            role(first_surface, WindowDecoration::ServerSide),
        );
        enqueue_surface_event(app.world_mut(), frame(first_surface, 320, 240));
        app.update();
        app.update();
        let first = app
            .world_mut()
            .query::<&OccupiesWindow>()
            .single(app.world())
            .expect("window")
            .0;
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(first)
                .expect("gaps kept")
                .position,
            Vec2::splat(8.0)
        );
        for expected in [
            BorderStyle::None,
            BorderStyle::Normal(7),
            BorderStyle::Pixel(7),
        ] {
            app.world_mut().trigger(BorderRequest(None));
            app.update();
            assert_eq!(
                app.world().get::<WindowBorderStyle>(first),
                Some(&WindowBorderStyle(expected))
            );
        }
        let second_surface = SurfaceId::for_test(304);
        enqueue_surface_event(
            app.world_mut(),
            role(second_surface, WindowDecoration::ServerSide),
        );
        enqueue_surface_event(app.world_mut(), frame(second_surface, 320, 240));
        app.update();
        app.update();
        let root = app
            .world()
            .get::<PrimaryWindowPresentation>(first)
            .expect("root")
            .entity();
        assert_eq!(
            app.world().get::<PresentationInsets>(root),
            Some(&BorderStyle::Pixel(7).insets())
        );
        app.world_mut()
            .entity_mut(first)
            .insert(weld_window::WindowVacancy::Retain);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface: first_surface,
                kind: HostSurfaceEventKind::Destroyed,
            },
        );
        app.update();
        app.update();
        assert!(
            app.world()
                .get::<weld_window::WindowOccupant>(first)
                .is_none()
        );
        assert!(
            app.world()
                .get::<weld_window::SoleTiledWindow>(first)
                .is_none(),
            "the vacant slot still shares its workspace with the second tile"
        );
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface: second_surface,
                kind: HostSurfaceEventKind::Destroyed,
            },
        );
        for _ in 0..3 {
            app.update();
        }
        assert!(
            app.world()
                .get::<weld_window::SoleTiledWindow>(first)
                .is_some()
        );
        let root = app
            .world()
            .get::<PrimaryWindowPresentation>(first)
            .expect("retained root")
            .entity();
        assert_eq!(
            app.world().get::<PresentationInsets>(root),
            Some(&PresentationInsets::default())
        );
        assert_eq!(
            app.world().get::<WindowBorderStyle>(first),
            Some(&WindowBorderStyle(BorderStyle::Pixel(7)))
        );
    }

    #[test]
    fn smart_borders_follow_tiled_cardinality_and_keep_explicit_style_and_headers() {
        let mut app = tiled_test_app();
        {
            let mut settings = app.world_mut().resource_mut::<SsdSettings>();
            settings.hide_solo_border = true;
            settings.tiled = BorderStyle::Pixel(4);
        }
        app.world_mut()
            .resource_mut::<weld_tile::TileSettings>()
            .hide_solo_gaps = true;
        let lookup = |app: &mut App, surface| {
            app.world_mut()
                .query::<(&ClientToplevel, &OccupiesWindow)>()
                .iter(app.world())
                .find(|(client, _)| client.surface == surface)
                .expect("window")
                .1
                .0
        };
        let insets = |app: &App, window| {
            let root = app
                .world()
                .get::<PrimaryWindowPresentation>(window)
                .expect("root")
                .entity();
            *app.world().get::<PresentationInsets>(root).expect("insets")
        };
        let first_surface = SurfaceId::for_test(301);
        enqueue_surface_event(
            app.world_mut(),
            role(first_surface, WindowDecoration::ServerSide),
        );
        enqueue_surface_event(app.world_mut(), frame(first_surface, 320, 240));
        app.update();
        app.update();
        let first = lookup(&mut app, first_surface);
        assert_eq!(insets(&app, first), PresentationInsets::default());
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(first)
                .expect("sole tile")
                .size,
            Vec2::new(1000.0, 800.0)
        );
        let second_surface = SurfaceId::for_test(302);
        enqueue_surface_event(
            app.world_mut(),
            role(second_surface, WindowDecoration::ServerSide),
        );
        enqueue_surface_event(app.world_mut(), frame(second_surface, 320, 240));
        app.update();
        app.update();
        let second = lookup(&mut app, second_surface);
        assert_eq!(
            insets(&app, first),
            PresentationInsets::new(4.0, 4.0, 4.0, 4.0)
        );
        assert_eq!(
            insets(&app, second),
            PresentationInsets::new(4.0, 4.0, 4.0, 4.0)
        );
        app.world_mut().trigger(weld_tile::TileFloatingRequest {
            window: Some(second),
            enabled: Some(true),
        });
        app.update();
        app.update();
        assert_eq!(insets(&app, first), PresentationInsets::default());
        assert_eq!(insets(&app, second), BorderStyle::Normal(3).insets());
        app.world_mut()
            .entity_mut(first)
            .insert(WindowBorderStyle(BorderStyle::Normal(5)));
        app.update();
        assert_eq!(
            insets(&app, first),
            PresentationInsets::new(0.0, HEADER_HEIGHT, 0.0, 0.0)
        );
        app.world_mut()
            .resource_mut::<SsdSettings>()
            .hide_solo_border = false;
        app.update();
        assert_eq!(insets(&app, first), BorderStyle::Normal(5).insets());
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(first)
                .expect("layout retained")
                .size,
            Vec2::new(1000.0, 800.0)
        );
    }

    #[test]
    fn tile_outer_rectangle_survives_live_frame_metrics_changes() {
        let mut app = tiled_test_app();
        let surface = SurfaceId::for_test(100);
        enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ServerSide));
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        app.update();
        let window = app
            .world_mut()
            .query::<&OccupiesWindow>()
            .single(app.world())
            .expect("occupancy")
            .0;
        let outer = *app.world().get::<WindowGeometry>(window).expect("geometry");
        take_surface_actions(app.world_mut());
        app.world_mut().resource_mut::<SsdSettings>().tiled = BorderStyle::Pixel(8);
        app.update();
        assert_eq!(
            *app.world().get::<WindowGeometry>(window).expect("geometry"),
            outer
        );
        assert!(
            take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
                surface,
                logical_size: (outer.size - Vec2::splat(16.0)).as_uvec2(),
                resizing: false,
                fullscreen: false,
            })
        );
        let dialog_surface = SurfaceId::for_test(103);
        let mut dialog_role = role(dialog_surface, WindowDecoration::ServerSide);
        if let HostSurfaceEventKind::Role(weld_client::ClientSurfaceRole::Toplevel(state)) =
            &mut dialog_role.kind
        {
            state.parent = Some(surface);
        }
        enqueue_surface_event(app.world_mut(), dialog_role);
        enqueue_surface_event(app.world_mut(), frame(dialog_surface, 320, 240));
        app.update();
        let dialog = app
            .world_mut()
            .query::<(&ClientToplevel, &OccupiesWindow)>()
            .iter(app.world())
            .find(|(client, _)| client.surface == dialog_surface)
            .expect("dialog")
            .1
            .0;
        let placed = *app
            .world()
            .get::<WindowGeometry>(dialog)
            .expect("dialog geometry");
        assert_eq!(
            placed.position,
            outer.position + (outer.size - placed.size) * 0.5
        );
        app.update();
        assert_eq!(
            *app.world()
                .get::<WindowGeometry>(dialog)
                .expect("settled dialog"),
            placed
        );
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface: dialog_surface,
                kind: HostSurfaceEventKind::Destroyed,
            },
        );
        app.update();
        let late_parent_surface = SurfaceId::for_test(104);
        let child_surface = SurfaceId::for_test(105);
        let mut child_role = role(child_surface, WindowDecoration::ServerSide);
        if let HostSurfaceEventKind::Role(weld_client::ClientSurfaceRole::Toplevel(state)) =
            &mut child_role.kind
        {
            state.parent = Some(late_parent_surface);
        }
        enqueue_surface_event(app.world_mut(), child_role);
        enqueue_surface_event(app.world_mut(), frame(child_surface, 200, 100));
        app.update();
        enqueue_surface_event(
            app.world_mut(),
            role(late_parent_surface, WindowDecoration::ServerSide),
        );
        enqueue_surface_event(app.world_mut(), frame(late_parent_surface, 320, 240));
        app.update();
        let lookup = |app: &mut App, surface| {
            app.world_mut()
                .query::<(&ClientToplevel, &OccupiesWindow)>()
                .iter(app.world())
                .find(|(client, _)| client.surface == surface)
                .expect("family window")
                .1
                .0
        };
        let child = lookup(&mut app, child_surface);
        let parent = lookup(&mut app, late_parent_surface);
        let parent = *app
            .world()
            .get::<WindowGeometry>(parent)
            .expect("parent geometry");
        let child = *app
            .world()
            .get::<WindowGeometry>(child)
            .expect("child geometry");
        assert_eq!(
            child.position,
            parent.position + (parent.size - child.size) * 0.5
        );
    }

    #[test]
    fn live_border_styles_preserve_client_size_and_remove_hidden_hit_areas() {
        let mut app = test_app();
        app.world_mut().resource_mut::<SsdSettings>().floating = BorderStyle::Pixel(5);
        let surface = SurfaceId::for_test(101);
        enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ServerSide));
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        app.update();
        app.update();
        let window = app
            .world_mut()
            .query::<&OccupiesWindow>()
            .single(app.world())
            .expect("occupancy")
            .0;
        let root = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("presentation")
            .entity();
        assert_eq!(
            app.world().get::<PresentationInsets>(root),
            Some(&PresentationInsets::new(5.0, 5.0, 5.0, 5.0))
        );
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("geometry")
                .size,
            Vec2::new(330.0, 250.0)
        );
        assert!(
            app.world_mut()
                .query_filtered::<&Node, With<FrameHeader>>()
                .iter(app.world())
                .all(|node| node.display == Display::None)
        );
        app.world_mut()
            .trigger(BorderRequest(Some(BorderStyle::None)));
        app.update();
        let bare = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("bare presentation")
            .entity();
        assert!(app.world().get_entity(root).is_err());
        assert_eq!(
            app.world().get::<PresentationInsets>(bare),
            Some(&PresentationInsets::default())
        );
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("geometry")
                .size,
            Vec2::new(320.0, 240.0)
        );
        assert!(
            app.world_mut()
                .query_filtered::<&Node, With<WindowResizeHandle>>()
                .iter(app.world())
                .all(|node| node.display == Display::None)
        );
        assert_eq!(
            app.world().get::<BoxShadow>(bare),
            Some(&BoxShadow::default())
        );
        app.world_mut()
            .trigger(BorderRequest(Some(BorderStyle::Normal(2))));
        app.update();
        let normal = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("normal presentation")
            .entity();
        assert_eq!(
            app.world().get::<PresentationInsets>(normal),
            Some(&PresentationInsets::new(2.0, 32.0, 2.0, 2.0))
        );
        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("geometry")
                .size,
            Vec2::new(324.0, 274.0)
        );
        app.world_mut().resource_mut::<SsdSettings>().floating = BorderStyle::Pixel(24);
        app.update();
        assert_eq!(
            app.world()
                .get::<PrimaryWindowPresentation>(window)
                .expect("override retained")
                .entity(),
            normal
        );
        app.world_mut()
            .trigger(BorderRequest(Some(BorderStyle::None)));
        app.update();
        app.world_mut().trigger(BorderRequest(None));
        app.update();
        assert_eq!(
            app.world().get::<WindowBorderStyle>(window),
            Some(&WindowBorderStyle(BorderStyle::Normal(24)))
        );
        let right = app
            .world_mut()
            .query::<(&WindowResizeHandle, &Node)>()
            .iter(app.world())
            .find(|(handle, _)| handle.0 == ToplevelResizeEdge::Right)
            .expect("right handle")
            .1;
        assert_eq!(right.right, px(-24.0));
        assert_eq!(right.width, px(24.0));
    }

    #[test]
    fn palette_reload_updates_existing_roots_and_interaction_indicator() {
        let mut app = test_app();
        let surface = SurfaceId::for_test(102);
        enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ServerSide));
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        app.update();
        let window = app
            .world_mut()
            .query::<&OccupiesWindow>()
            .single(app.world())
            .expect("occupancy")
            .0;
        let root = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("presentation")
            .entity();
        let colors = FrameColors {
            border: Color::srgb(1.0, 0.0, 0.0),
            background: Color::srgb(0.0, 1.0, 0.0),
            foreground: Color::BLACK,
            indicator: Color::WHITE,
            child_border: Color::srgb(0.0, 0.0, 1.0),
        };
        app.world_mut().resource_mut::<SsdSettings>().focused = colors;
        app.update();
        assert_eq!(
            app.world()
                .get::<PrimaryWindowPresentation>(window)
                .expect("same root")
                .entity(),
            root
        );
        assert_eq!(
            app.world().get::<BorderColor>(root),
            Some(&BorderColor {
                top: colors.border,
                right: colors.child_border,
                bottom: colors.child_border,
                left: colors.child_border
            })
        );
        assert!(
            app.world_mut()
                .query_filtered::<&BackgroundColor, With<FrameHeader>>()
                .iter(app.world())
                .all(|color| color.0 == colors.background)
        );
        app.world_mut().trigger(weld_window::WindowCommand {
            window,
            kind: weld_window::WindowCommandKind::BeginInteraction(WindowInteractionKind::Move),
        });
        app.update();
        assert_eq!(
            app.world().get::<BorderColor>(root),
            Some(&BorderColor::all(colors.indicator))
        );
    }

    #[test]
    fn relocated_provenance_uses_the_hoist_accent_in_both_focus_states() {
        let mut app = test_app();
        let source = ClientSourceId::new(1);
        assert!(register_client_source(
            app.world_mut(),
            ClientSourceDescriptor::new(source, ClientProvenance::Relocated),
        ));
        let relocated = SurfaceId::new(ClientId::new(source, 1), 1);
        enqueue_surface_event(
            app.world_mut(),
            role(relocated, WindowDecoration::ServerSide),
        );
        enqueue_surface_event(app.world_mut(), frame(relocated, 320, 240));
        app.update();
        let relocated_window = app
            .world_mut()
            .query::<(&ClientToplevel, &OccupiesWindow)>()
            .single(app.world())
            .map(|(_, occupancy)| occupancy.0)
            .expect("relocated window");
        let root = app
            .world()
            .get::<PrimaryWindowPresentation>(relocated_window)
            .expect("relocated SSD")
            .entity();
        assert_eq!(
            app.world().get::<BorderColor>(root),
            Some(&BorderColor::all(RELOCATED_FOCUSED_BORDER))
        );

        let local = SurfaceId::for_test(2);
        enqueue_surface_event(app.world_mut(), role(local, WindowDecoration::ServerSide));
        enqueue_surface_event(app.world_mut(), frame(local, 320, 240));
        app.update();
        assert_eq!(
            app.world().get::<BorderColor>(root),
            Some(&BorderColor::all(RELOCATED_UNFOCUSED_BORDER))
        );
    }

    fn write_primary_button(app: &mut App, state: ButtonState) {
        app.world_mut().write_message(MouseButtonInput {
            button: MouseButton::Left,
            state,
            window: bevy::ecs::entity::Entity::PLACEHOLDER,
        });
    }

    fn write_mouse_motion(app: &mut App, delta: Vec2) {
        app.world_mut().write_message(MouseMotion { delta });
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

    fn role(surface: SurfaceId, decoration: WindowDecoration) -> HostSurfaceEvent {
        HostSurfaceEvent {
            surface,
            kind: HostSurfaceEventKind::Role(weld_client::ClientSurfaceRole::Toplevel(
                weld_client::ToplevelState {
                    parent: None,
                    decoration,
                },
            )),
        }
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
            kind: HostSurfaceEventKind::Commit(SurfaceTreeSnapshot {
                client_mapped: true,
                alpha_mode: Default::default(),
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
            kind: HostSurfaceEventKind::Commit(SurfaceTreeSnapshot {
                client_mapped: false,
                alpha_mode: Default::default(),
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
        let surface = SurfaceId::for_test(41);
        enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ClientSide));
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

        enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ServerSide));
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
        write_primary_button(&mut app, ButtonState::Pressed);
        app.update();
        write_mouse_motion(&mut app, Vec2::new(12.0, 8.0));
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
        write_primary_button(&mut app, ButtonState::Released);
        app.update();

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
        let surface = SurfaceId::for_test(49);
        enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ServerSide));
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        app.update();

        let window = app
            .world_mut()
            .query::<(&ClientToplevel, &OccupiesWindow)>()
            .single(app.world())
            .expect("server-decorated toplevel should be admitted")
            .1
            .0;
        let root = app
            .world()
            .get::<PrimaryWindowPresentation>(window)
            .expect("SSD should claim the window")
            .entity();
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
        write_primary_button(&mut app, ButtonState::Pressed);
        app.update();
        assert!(matches!(
            app.world().get::<WindowInteractionSession>(window),
            Some(WindowInteractionSession {
                kind: weld_window::WindowInteractionKind::Resize(ToplevelResizeEdge::Right),
                ..
            })
        ));

        write_mouse_motion(&mut app, Vec2::new(20.0, 0.0));
        app.update();

        assert_eq!(
            app.world()
                .get::<WindowGeometry>(window)
                .expect("border resize should update desired outer geometry")
                .size,
            outer_size + Vec2::new(20.0, 0.0)
        );
        let root_node = app
            .world()
            .get::<Node>(root)
            .expect("SSD root should retain layout");
        assert_eq!(
            (root_node.width, root_node.height),
            (px(outer_size.x + 20.0), px(outer_size.y))
        );
        let (_, content_node) = app
            .world_mut()
            .query::<(&SurfaceNode, &Node)>()
            .single(app.world())
            .expect("SSD should retain its last committed client surface");
        assert_eq!(
            (content_node.width, content_node.height),
            (px(320.0), px(240.0))
        );
        assert_eq!(content_node.flex_shrink, 0.0);
        assert!(
            take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
                surface,
                logical_size: UVec2::new(340, 240),
                resizing: true,
                fullscreen: false,
            })
        );

        write_primary_button(&mut app, ButtonState::Released);
        app.update();
        assert!(
            take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
                surface,
                logical_size: UVec2::new(340, 240),
                resizing: false,
                fullscreen: false,
            })
        );
    }

    #[test]
    fn client_resize_updates_desired_geometry_and_preserves_the_left_anchor() {
        let mut app = test_app();
        let surface = SurfaceId::for_test(42);
        enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ClientSide));
        enqueue_surface_event(app.world_mut(), frame(surface, 320, 240));
        app.update();
        let window = {
            let mut toplevels = app
                .world_mut()
                .query::<(&ClientToplevel, &OccupiesWindow)>();
            let (_, occupancy) = toplevels
                .single(app.world())
                .expect("mapped toplevel should be admitted");
            let window = occupancy.0;
            app.world_mut()
                .query_filtered::<bevy::ecs::entity::Entity, With<SurfaceNode>>()
                .single(app.world())
                .expect("client presentation should mount its surface");
            window
        };
        let initial = *app
            .world()
            .get::<WindowGeometry>(window)
            .expect("floating manager should initialize geometry");
        take_surface_actions(app.world_mut());

        write_primary_button(&mut app, ButtonState::Pressed);
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface,
                kind: HostSurfaceEventKind::Interaction(ToplevelInteractionRequestKind::Resize {
                    edges: ToplevelResizeEdge::Left,
                }),
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
        assert!(
            take_surface_actions(app.world_mut()).contains(&SurfaceAction::Resize {
                surface,
                logical_size: UVec2::new(320, 240),
                resizing: true,
                fullscreen: false,
            })
        );

        write_mouse_motion(&mut app, Vec2::new(10.0, 0.0));
        write_mouse_motion(&mut app, Vec2::new(10.0, 0.0));
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
                resizing: true,
                fullscreen: false,
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
                kind: HostSurfaceEventKind::Interaction(ToplevelInteractionRequestKind::End),
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

    mod opaque;

    #[test]
    fn popup_reparents_when_the_owner_changes_presentation() {
        let mut app = test_app();
        let owner = SurfaceId::for_test(43);
        let popup = SurfaceId::for_test(44);
        enqueue_surface_event(app.world_mut(), role(owner, WindowDecoration::ClientSide));
        enqueue_surface_event(
            app.world_mut(),
            frame_with_geometry(owner, 360, 276, Vec2::new(20.0, 18.0), UVec2::new(320, 240)),
        );
        enqueue_surface_event(
            app.world_mut(),
            HostSurfaceEvent {
                surface: popup,
                kind: HostSurfaceEventKind::Role(weld_client::ClientSurfaceRole::Popup(
                    weld_client::PopupState {
                        owner,
                        position: weld_client::LogicalPoint::new(102.0, 52.0),
                        stack_index: 1,
                    },
                )),
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

        enqueue_surface_event(app.world_mut(), role(owner, WindowDecoration::ServerSide));
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
    fn committed_csd_overflow_changes_without_overwriting_desired_geometry() {
        let mut app = test_app();
        let surface = SurfaceId::for_test(45);
        enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ClientSide));
        enqueue_surface_event(
            app.world_mut(),
            frame_with_geometry(
                surface,
                360,
                278,
                Vec2::new(20.0, 18.0),
                UVec2::new(320, 240),
            ),
        );
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
        let root_node = app
            .world()
            .get::<Node>(root)
            .expect("presentation should have layout");
        assert_eq!((root_node.width, root_node.height), (px(320.0), px(240.0)));

        enqueue_surface_event(
            app.world_mut(),
            frame_with_geometry(
                surface,
                440,
                318,
                Vec2::new(20.0, 18.0),
                UVec2::new(400, 280),
            ),
        );
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
            (px(440.0), px(318.0))
        );
        assert_eq!(surface_node.1.flex_shrink, 0.0);
        let root_node = app
            .world()
            .get::<Node>(root)
            .expect("presentation should retain layout");
        assert_eq!((root_node.width, root_node.height), (px(320.0), px(240.0)));
    }

    #[test]
    fn unmap_hides_a_window_and_surface_destruction_removes_the_default_frame() {
        let mut app = test_app();
        let surface = SurfaceId::for_test(46);
        enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ClientSide));
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
        let first = SurfaceId::for_test(47);
        let second = SurfaceId::for_test(48);
        for surface in [first, second] {
            enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ClientSide));
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
        app.add_plugins(weld_window::fullscreen::FullscreenPlugin);
        let surface = SurfaceId::for_test(91);
        enqueue_surface_event(app.world_mut(), role(surface, WindowDecoration::ServerSide));
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
        let fullscreen_surface = SurfaceId::for_test(92);
        enqueue_surface_event(
            app.world_mut(),
            role(fullscreen_surface, WindowDecoration::ServerSide),
        );
        enqueue_surface_event(app.world_mut(), frame(fullscreen_surface, 320, 240));
        app.update();
        let owner = app
            .world_mut()
            .query::<(&ClientToplevel, &OccupiesWindow)>()
            .iter(app.world())
            .find(|(client, _)| client.surface == fullscreen_surface)
            .expect("owner")
            .1
            .0;
        app.world_mut()
            .trigger(weld_window::fullscreen::FullscreenRequest {
                window: Some(owner),
                action: weld_window::fullscreen::FullscreenAction::Enable(
                    weld_window::fullscreen::FullscreenMode::Normal,
                ),
            });
        app.update();
        assert_eq!(
            app.world()
                .get::<Node>(primary_root)
                .expect("overlap projection")
                .display,
            Display::None
        );
        assert!(
            app.world()
                .get::<weld_window::fullscreen::FullscreenOccluded>(window)
                .is_none(),
            "foreign home output remains available"
        );
    }
}
