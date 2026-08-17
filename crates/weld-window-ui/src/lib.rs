//! Reusable, unstyled Bevy UI presentation for managed windows.

use std::collections::HashSet;

const PROFILE_TARGET: &str = "weld_profile";

mod client;
mod interaction;
mod mount;
mod popup;

pub use mount::{surface_content, surface_content_with_node};

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        component::Component,
        entity::Entity,
        hierarchy::ChildOf,
        query::{With, Without},
        schedule::IntoScheduleConfigs,
        system::{Commands, Query},
    },
    math::Vec2,
    prelude::{BoxShadow, Display, GlobalZIndex, Node, UiTargetCamera, ZIndex, px},
    scene::CommandsSceneExt,
    window::RequestRedraw,
};
use weld_app::{
    cursor::{CursorRequest, CursorSystems},
    output::{OutputCompositionCamera, OutputPosition, PrimaryOutput, WeldOutput},
    surface::{ClientDecorated, ClientPopup, ClientSurface, ClientToplevel, MappedSurface},
};
use weld_window::{
    OccupiesWindow, PresentationInsets, PresentationOffset, PresentsWindow,
    PrimaryWindowPresentation, WindowGeometry, WindowGeometryAnchor, WindowOccupant, WindowOutput,
    WindowOutputIntersections, WindowProjection, WindowSystems, WindowVacancy, WindowVisibility,
    WindowZOrder,
};

/// Attaches a UI root to the client-surface entity it presents.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
#[relationship(relationship_target = PrimarySurfacePresentation)]
pub struct PresentsSurface(pub Entity);

/// The primary UI presentation of a popup or other non-window surface.
#[derive(Component, Debug)]
#[relationship_target(relationship = PresentsSurface, linked_spawn)]
pub struct PrimarySurfacePresentation(Entity);

impl PrimarySurfacePresentation {
    pub fn entity(&self) -> Entity {
        self.0
    }
}

#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
struct PopupProjection {
    source: Entity,
    window: Entity,
    output: Entity,
}

/// Installs baseline CSD, popup, and reusable window-UI behavior.
pub struct WindowUiPlugin;

impl Plugin for WindowUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<CursorRequest>()
            .configure_sets(
                PreUpdate,
                WindowSystems::InteractionFinalize.before(CursorSystems::Resolve),
            )
            .add_systems(
                PreUpdate,
                (
                    interaction::attach_resize_cursor_icons,
                    interaction::request_interaction_cursor
                        .after(WindowSystems::InteractionFinalize)
                        .before(CursorSystems::Resolve),
                ),
            )
            .add_systems(
                PreUpdate,
                revoke_client_presentations.in_set(WindowSystems::PresentationRevoke),
            )
            .add_systems(
                PreUpdate,
                present_client_windows.in_set(WindowSystems::PresentationClaim),
            )
            .add_systems(
                PreUpdate,
                (sync_client_presentation_metrics, present_popups)
                    .in_set(WindowSystems::PresentationMetrics),
            )
            .add_systems(
                PreUpdate,
                (
                    reconcile_client_window_projections,
                    sync_window_roots,
                    sync_popup_presentations,
                    reconcile_popup_projections,
                )
                    .chain()
                    .in_set(WindowSystems::UiReconcile),
            )
            .add_systems(
                PreUpdate,
                sync_window_roots.in_set(WindowSystems::FinalReconcile),
            );
    }
}

fn revoke_client_presentations(
    mut commands: Commands,
    roots: Query<(Entity, &WindowProjection), With<client::ClientWindowPresentation>>,
    windows: Query<&WindowOccupant>,
    occupants: Query<(), With<ClientDecorated>>,
) {
    for (root, projection) in &roots {
        let still_client_decorated = windows
            .get(projection.window())
            .ok()
            .is_some_and(|occupant| occupants.contains(occupant.entity()));
        if !still_client_decorated {
            commands.entity(root).despawn();
        }
    }
}

fn reconcile_client_window_projections(
    mut commands: Commands,
    windows: Query<(
        Entity,
        &PrimaryWindowPresentation,
        &WindowOccupant,
        &WindowZOrder,
        &WindowOutputIntersections,
    )>,
    occupants: Query<(&ClientToplevel, &MappedSurface, Option<&ClientDecorated>)>,
    outputs: Query<&OutputCompositionCamera>,
    roots: Query<(Entity, &WindowProjection), With<client::ClientWindowPresentation>>,
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
        let Ok((toplevel, mapped, Some(_))) = occupants.get(occupant.entity()) else {
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
                .spawn_scene(client::scene(toplevel.surface))
                .insert((
                    WindowProjection::new(window, output),
                    UiTargetCamera(camera),
                    client::ClientWindowPresentation,
                    PresentationOffset(mapped.visual_offset),
                    PresentationInsets::default(),
                    WindowGeometryAnchor(-mapped.visual_offset),
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

fn present_client_windows(
    mut commands: Commands,
    windows: Query<
        (
            Entity,
            &WindowOccupant,
            &WindowZOrder,
            Option<&WindowOutput>,
        ),
        Without<PrimaryWindowPresentation>,
    >,
    occupants: Query<(&ClientToplevel, &MappedSurface, Option<&ClientDecorated>)>,
    outputs: OutputCameraQuery,
) {
    let _presentation_span =
        tracing::trace_span!(target: PROFILE_TARGET, "weld_window_present_client_windows")
            .entered();
    for (window, occupant, z_order, output) in &windows {
        let Ok((toplevel, mapped, Some(_))) = occupants.get(occupant.entity()) else {
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
            .spawn_scene(client::scene(toplevel.surface))
            .insert((
                PresentsWindow(window),
                WindowProjection::new(window, output),
                client::ClientWindowPresentation,
                PresentationOffset(mapped.visual_offset),
                PresentationInsets::default(),
                WindowGeometryAnchor(-mapped.visual_offset),
                GlobalZIndex(z_order.0),
            ))
            .id();
        if let Some(camera) = camera {
            commands.entity(root).insert(UiTargetCamera(camera));
        }
    }
}

type ClientMetricRoots<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static PresentsWindow,
        &'static mut PresentationOffset,
        &'static mut WindowGeometryAnchor,
        Option<&'static BoxShadow>,
    ),
    With<client::ClientWindowPresentation>,
>;

fn sync_client_presentation_metrics(
    mut commands: Commands,
    windows: Query<&WindowOccupant>,
    occupants: Query<&MappedSurface>,
    mut roots: ClientMetricRoots,
) {
    for (root, presentation, mut offset, mut anchor, shadow) in &mut roots {
        let Some(mapped) = windows
            .get(presentation.0)
            .ok()
            .and_then(|occupant| occupants.get(occupant.entity()).ok())
        else {
            continue;
        };
        offset.0 = mapped.visual_offset;
        anchor.0 = -mapped.visual_offset;
        match (mapped.has_visual_overflow(), shadow.is_some()) {
            (true, true) => {
                commands.entity(root).remove::<BoxShadow>();
            }
            (false, false) => {
                commands.entity(root).insert(client::fallback_shadow());
            }
            _ => {}
        }
    }
}

type WindowRootStateQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static WindowGeometry,
        &'static WindowOutput,
        &'static WindowVisibility,
        &'static WindowZOrder,
        &'static WindowVacancy,
        Option<&'static WindowOccupant>,
    ),
>;

fn sync_window_roots(
    windows: WindowRootStateQuery,
    occupants: Query<Option<&MappedSurface>>,
    mut roots: Query<(
        &WindowProjection,
        Option<&PresentationOffset>,
        &mut GlobalZIndex,
        &mut Node,
    )>,
    output_positions: Query<&OutputPosition>,
    mut redraw: bevy::ecs::message::MessageWriter<RequestRedraw>,
) {
    let mut changed = false;
    for (projection, offset, mut z_index, mut node) in &mut roots {
        let Ok((geometry, home, visibility, window_z, vacancy, occupant)) =
            windows.get(projection.window())
        else {
            continue;
        };
        let (Ok(home_position), Ok(target_position)) = (
            output_positions.get(home.0),
            output_positions.get(projection.output()),
        ) else {
            continue;
        };
        let presentable = occupant.map_or(*vacancy == WindowVacancy::Retain, |occupant| {
            occupants
                .get(occupant.entity())
                .is_ok_and(|mapped| mapped.is_some())
        });
        let visible = *visibility == WindowVisibility::Visible && presentable;
        let display = if visible {
            Display::Flex
        } else {
            Display::None
        };
        let position = home_position.0 + geometry.position - target_position.0
            + offset.copied().unwrap_or_default().0;
        if node.display != display || node.left != px(position.x) || node.top != px(position.y) {
            node.display = display;
            node.left = px(position.x);
            node.top = px(position.y);
            changed = true;
        }
        if z_index.0 != window_z.0 {
            z_index.0 = window_z.0;
            changed = true;
        }
    }
    if changed {
        redraw.write(RequestRedraw);
    }
}

fn present_popups(
    mut commands: Commands,
    popups: Query<
        (Entity, &ClientSurface, &ClientPopup, &MappedSurface),
        Without<PrimarySurfacePresentation>,
    >,
    toplevels: Query<(&ClientToplevel, &OccupiesWindow)>,
    windows: Query<(
        &PrimaryWindowPresentation,
        &WindowVisibility,
        Option<&WindowOccupant>,
    )>,
    anchors: Query<&WindowGeometryAnchor>,
    window_projections: Query<&WindowProjection>,
) {
    for (source, client_surface, popup, mapped) in &popups {
        let Some(window) = toplevels.iter().find_map(|(toplevel, occupancy)| {
            (toplevel.surface == popup.owner).then_some(occupancy.0)
        }) else {
            continue;
        };
        let Ok((presentation, WindowVisibility::Visible, Some(_))) = windows.get(window) else {
            continue;
        };
        let Ok(anchor) = anchors.get(presentation.entity()) else {
            continue;
        };
        let Ok(window_projection) = window_projections.get(presentation.entity()) else {
            continue;
        };
        let position = popup_position(*popup, *mapped, *anchor);
        commands
            .spawn_scene(popup::scene(client_surface.surface))
            .insert((
                PresentsSurface(source),
                PopupProjection {
                    source,
                    window,
                    output: window_projection.output(),
                },
                popup::PopupPresentation,
                ChildOf(presentation.entity()),
                ZIndex(popup.stack_index),
                popup_node(position, true),
            ));
    }
}

type PopupRoots<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static PresentsSurface,
        &'static mut Node,
        &'static mut ZIndex,
        Option<&'static ChildOf>,
    ),
    With<popup::PopupPresentation>,
>;

fn sync_popup_presentations(
    mut commands: Commands,
    popups: Query<(
        &ClientPopup,
        Option<&MappedSurface>,
        &PrimarySurfacePresentation,
    )>,
    toplevels: Query<(&ClientToplevel, &OccupiesWindow)>,
    windows: Query<(
        &PrimaryWindowPresentation,
        &WindowVisibility,
        Option<&WindowOccupant>,
    )>,
    anchors: Query<&WindowGeometryAnchor>,
    mut roots: PopupRoots,
) {
    for (root, claim, mut node, mut z_index, parent) in &mut roots {
        let Ok((popup, Some(mapped), primary)) = popups.get(claim.0) else {
            node.display = Display::None;
            continue;
        };
        if primary.entity() != root {
            continue;
        }
        let Some(window) = toplevels.iter().find_map(|(toplevel, occupancy)| {
            (toplevel.surface == popup.owner).then_some(occupancy.0)
        }) else {
            node.display = Display::None;
            continue;
        };
        let Ok((presentation, WindowVisibility::Visible, Some(_))) = windows.get(window) else {
            node.display = Display::None;
            continue;
        };
        let Ok(anchor) = anchors.get(presentation.entity()) else {
            node.display = Display::None;
            continue;
        };
        if parent.is_none_or(|parent| parent.parent() != presentation.entity()) {
            commands.entity(root).insert(ChildOf(presentation.entity()));
        }
        let expected = popup_node(popup_position(*popup, *mapped, *anchor), true);
        if *node != expected {
            *node = expected;
        }
        if z_index.0 != popup.stack_index {
            z_index.0 = popup.stack_index;
        }
    }
}

type PopupProjectionRoots<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static PopupProjection,
        &'static mut Node,
        &'static mut ZIndex,
        Option<&'static ChildOf>,
    ),
    With<popup::PopupPresentation>,
>;

fn reconcile_popup_projections(
    mut commands: Commands,
    popups: Query<(Entity, &ClientSurface, &ClientPopup, Option<&MappedSurface>)>,
    toplevels: Query<(&ClientToplevel, &OccupiesWindow)>,
    windows: Query<(&WindowVisibility, Option<&WindowOccupant>)>,
    window_roots: Query<(Entity, &WindowProjection, &WindowGeometryAnchor)>,
    mut roots: PopupProjectionRoots,
) {
    for (root, projection, mut node, mut z_index, parent) in &mut roots {
        let Ok((_, _, popup, Some(mapped))) = popups.get(projection.source) else {
            commands.entity(root).despawn();
            continue;
        };
        let Some((window_root, _, anchor)) = window_roots.iter().find(|(_, window, _)| {
            window.window() == projection.window && window.output() == projection.output
        }) else {
            commands.entity(root).despawn();
            continue;
        };
        let visible = windows
            .get(projection.window)
            .ok()
            .is_some_and(|(visibility, occupant)| {
                *visibility == WindowVisibility::Visible && occupant.is_some()
            });
        if parent.is_none_or(|parent| parent.parent() != window_root) {
            commands.entity(root).insert(ChildOf(window_root));
        }
        let expected = popup_node(popup_position(*popup, *mapped, *anchor), visible);
        if *node != expected {
            *node = expected;
        }
        if z_index.0 != popup.stack_index {
            z_index.0 = popup.stack_index;
        }
    }

    for (source, client_surface, popup, mapped) in &popups {
        let Some(mapped) = mapped else {
            continue;
        };
        let Some(window) = toplevels.iter().find_map(|(toplevel, occupancy)| {
            (toplevel.surface == popup.owner).then_some(occupancy.0)
        }) else {
            continue;
        };
        for (window_root, projection, anchor) in window_roots
            .iter()
            .filter(|(_, projection, _)| projection.window() == window)
        {
            if roots.iter().any(|(_, existing, _, _, _)| {
                existing.source == source && existing.output == projection.output()
            }) {
                continue;
            }
            commands
                .spawn_scene(popup::scene(client_surface.surface))
                .insert((
                    PopupProjection {
                        source,
                        window,
                        output: projection.output(),
                    },
                    popup::PopupPresentation,
                    ChildOf(window_root),
                    ZIndex(popup.stack_index),
                    popup_node(popup_position(*popup, *mapped, *anchor), true),
                ));
        }
    }
}

fn popup_position(popup: ClientPopup, mapped: MappedSurface, anchor: WindowGeometryAnchor) -> Vec2 {
    anchor.0 + popup.position + mapped.visual_offset
}

fn popup_node(position: Vec2, visible: bool) -> Node {
    Node {
        position_type: bevy::ui::PositionType::Absolute,
        left: px(position.x),
        top: px(position.y),
        display: if visible {
            Display::Flex
        } else {
            Display::None
        },
        ..Default::default()
    }
}
