//! Reusable, unstyled Bevy UI presentation for managed windows.

const PROFILE_TARGET: &str = "weld_profile";

mod client;
mod interaction;
mod mount;
mod popup;

pub use interaction::{WindowMoveHandle, WindowResizeFrame, WindowResizeHandle};
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
    prelude::{BoxShadow, Display, GlobalZIndex, Node, ZIndex, px},
    scene::CommandsSceneExt,
    window::RequestRedraw,
};
use weld_app::{
    composition::composition_advance_requested,
    cursor::{CursorRequest, CursorSystems},
    surface::{ClientDecorated, ClientPopup, ClientSurface, ClientToplevel, MappedSurface},
};
use weld_window::{
    OccupiesWindow, PresentationInsets, PresentationOffset, PresentsWindow,
    PrimaryWindowPresentation, WindowGeometry, WindowGeometryAnchor, WindowOccupant, WindowSystems,
    WindowVisibility, WindowZOrder,
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

/// Installs baseline CSD, popup, and reusable window-UI behavior.
pub struct WindowUiPlugin;

impl Plugin for WindowUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<CursorRequest>()
            .add_observer(interaction::activate_window)
            .add_observer(interaction::begin_move_handle)
            .add_observer(interaction::begin_resize_frame)
            .add_observer(interaction::begin_resize_handle)
            .add_observer(interaction::drag_window)
            .add_observer(interaction::end_drag)
            .add_observer(interaction::cancel_drag)
            .add_systems(
                PreUpdate,
                (
                    interaction::attach_resize_cursor_icons,
                    interaction::request_interaction_cursor.before(CursorSystems::Resolve),
                ),
            )
            .add_systems(
                PreUpdate,
                revoke_client_presentations
                    .run_if(composition_advance_requested)
                    .in_set(WindowSystems::PresentationRevoke),
            )
            .add_systems(
                PreUpdate,
                present_client_windows
                    .run_if(composition_advance_requested)
                    .in_set(WindowSystems::PresentationClaim),
            )
            .add_systems(
                PreUpdate,
                (sync_client_presentation_metrics, present_popups)
                    .run_if(composition_advance_requested)
                    .in_set(WindowSystems::PresentationMetrics),
            )
            .add_systems(
                PreUpdate,
                (sync_window_roots, sync_popup_presentations)
                    .chain()
                    .run_if(composition_advance_requested)
                    .in_set(WindowSystems::UiReconcile),
            )
            .add_systems(
                PreUpdate,
                sync_window_roots
                    .run_if(composition_advance_requested)
                    .in_set(WindowSystems::FinalReconcile),
            );
    }
}

fn revoke_client_presentations(
    mut commands: Commands,
    roots: Query<(Entity, &PresentsWindow), With<client::ClientWindowPresentation>>,
    windows: Query<&WindowOccupant>,
    occupants: Query<(), With<ClientDecorated>>,
) {
    for (root, presentation) in &roots {
        let still_client_decorated = windows
            .get(presentation.0)
            .ok()
            .is_some_and(|occupant| occupants.contains(occupant.entity()));
        if !still_client_decorated {
            commands.entity(root).despawn();
        }
    }
}

fn present_client_windows(
    mut commands: Commands,
    windows: Query<(Entity, &WindowOccupant, &WindowZOrder), Without<PrimaryWindowPresentation>>,
    occupants: Query<(&ClientToplevel, &MappedSurface, Option<&ClientDecorated>)>,
) {
    let _presentation_span =
        tracing::trace_span!(target: PROFILE_TARGET, "weld_window_present_client_windows")
            .entered();
    for (window, occupant, z_order) in &windows {
        let Ok((toplevel, mapped, Some(_))) = occupants.get(occupant.entity()) else {
            continue;
        };
        commands
            .spawn_scene(client::scene(toplevel.surface))
            .insert((
                PresentsWindow(window),
                client::ClientWindowPresentation,
                PresentationOffset(mapped.visual_offset),
                PresentationInsets::default(),
                WindowGeometryAnchor(-mapped.visual_offset),
                GlobalZIndex(z_order.0),
            ));
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

fn sync_window_roots(
    windows: Query<(
        &WindowGeometry,
        &WindowVisibility,
        &WindowZOrder,
        Option<&WindowOccupant>,
    )>,
    occupants: Query<Option<&MappedSurface>>,
    mut roots: Query<(
        &PresentsWindow,
        Option<&PresentationOffset>,
        &mut GlobalZIndex,
        &mut Node,
    )>,
    mut redraw: bevy::ecs::message::MessageWriter<RequestRedraw>,
) {
    let mut changed = false;
    for (presentation, offset, mut z_index, mut node) in &mut roots {
        let Ok((geometry, visibility, window_z, occupant)) = windows.get(presentation.0) else {
            continue;
        };
        let mapped = occupant
            .and_then(|occupant| occupants.get(occupant.entity()).ok())
            .is_some_and(|mapped| mapped.is_some());
        let visible = *visibility == WindowVisibility::Visible && mapped;
        let display = if visible {
            Display::Flex
        } else {
            Display::None
        };
        let position = geometry.position + offset.copied().unwrap_or_default().0;
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
        let position = popup_position(*popup, *mapped, *anchor);
        commands
            .spawn_scene(popup::scene(client_surface.surface))
            .insert((
                PresentsSurface(source),
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
