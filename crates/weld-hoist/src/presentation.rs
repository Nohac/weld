//! Hoist-owned source placeholder and reclaim presentation.

use std::collections::HashSet;

use bevy::{
    ecs::{
        component::Component,
        entity::Entity,
        message::MessageWriter,
        observer::On,
        query::With,
        system::{Commands, Query},
        template::template,
    },
    picking::{
        Pickable,
        events::{Click, Pointer},
        pointer::PointerButton,
    },
    prelude::{
        AccessibleLabel, AlignItems, BackgroundColor, BorderRadius, Button, Children, Color,
        FlexDirection, GlobalZIndex, JustifyContent, Node, Overflow, PositionType, Scene,
        UiTargetCamera, px,
    },
    scene::{CommandsSceneExt, bsn},
    text::{FontSourceTemplate, TextFont},
    window::RequestRedraw,
};
use weld_app::output::{OutputCompositionCamera, PrimaryOutput, WeldOutput};
use weld_window::{
    ManagedWindow, PresentsWindow, PrimaryWindowPresentation, WindowGeometry, WindowOutput,
    WindowOutputIntersections, WindowPresentationOverride, WindowProjection, WindowZOrder,
};

use crate::{
    DismissHoistTombstone, HoistSession, HoistSessionPhase, HoistSourceMode, ReclaimHoist,
};

#[derive(Component, Clone, Copy, Debug)]
pub(super) struct ReclaimHoistHandle(Entity);

#[derive(Component, Clone, Copy, Debug)]
pub(super) struct DismissHoistTombstoneHandle(Entity);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HoistPlaceholderState {
    Live,
    Closed,
}

#[derive(Component, Clone, Copy, Debug)]
pub(super) struct HoistPresentation {
    session: Entity,
    pub(super) state: HoistPlaceholderState,
}

pub(super) fn request_tombstone_dismissal(
    mut click: On<Pointer<Click>>,
    handles: Query<&DismissHoistTombstoneHandle>,
    mut dismissals: MessageWriter<DismissHoistTombstone>,
) {
    if click.button != PointerButton::Primary || click.original_event_target() != click.entity {
        return;
    }
    let Ok(handle) = handles.get(click.entity) else {
        return;
    };
    click.propagate(false);
    dismissals.write(DismissHoistTombstone { session: handle.0 });
}

pub(super) fn request_reclaim(
    mut click: On<Pointer<Click>>,
    handles: Query<&ReclaimHoistHandle>,
    mut reclaims: MessageWriter<ReclaimHoist>,
) {
    if click.button != PointerButton::Primary || click.original_event_target() != click.entity {
        return;
    }
    let Ok(handle) = handles.get(click.entity) else {
        return;
    };
    click.propagate(false);
    reclaims.write(ReclaimHoist { session: handle.0 });
}

pub(super) fn revoke_hoist_presentations(
    mut commands: Commands,
    roots: Query<(Entity, &WindowProjection, &HoistPresentation)>,
    sessions: Query<&HoistSession>,
    windows: Query<&WindowPresentationOverride>,
) {
    for (root, projection, presentation) in &roots {
        let valid = sessions.get(presentation.session).is_ok_and(|session| {
            session.source_mode == HoistSourceMode::PreservedSlot
                && presentation.state == placeholder_state(session.phase)
                && session.source == projection.window()
                && windows
                    .get(session.source)
                    .is_ok_and(|owner| owner.owner() == presentation.session)
        });
        if !valid {
            commands.entity(root).despawn();
        }
    }
}

type HoistWindowQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static WindowZOrder,
        Option<&'static WindowOutput>,
        &'static WindowOutputIntersections,
        Option<&'static PrimaryWindowPresentation>,
        &'static WindowPresentationOverride,
    ),
    With<ManagedWindow>,
>;

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

pub(super) fn present_hoist_windows(
    mut commands: Commands,
    sessions: Query<(Entity, &HoistSession)>,
    windows: HoistWindowQuery,
    outputs: OutputCameraQuery,
) {
    for (session_entity, session) in &sessions {
        if session.source_mode != HoistSourceMode::PreservedSlot {
            continue;
        }
        let Ok((z_order, output, _, primary, owner)) = windows.get(session.source) else {
            continue;
        };
        if primary.is_some() || owner.owner() != session_entity {
            continue;
        }
        let output = output.map(|output| output.0).or_else(|| {
            outputs
                .iter()
                .find_map(|(output, _, primary)| primary.is_some().then_some(output))
        });
        let Some(output) = output else {
            continue;
        };
        spawn_placeholder(
            &mut commands,
            session_entity,
            session,
            output,
            z_order.0,
            true,
            &outputs,
        );
    }
}

pub(super) fn reconcile_hoist_projections(
    mut commands: Commands,
    sessions: Query<(Entity, &HoistSession)>,
    windows: HoistWindowQuery,
    outputs: OutputCameraQuery,
    roots: Query<(Entity, &WindowProjection, &HoistPresentation)>,
) {
    let mut retained = HashSet::new();
    let mut ordered_roots = roots.iter().collect::<Vec<_>>();
    ordered_roots.sort_unstable_by_key(|(root, projection, _)| {
        let secondary = windows
            .get(projection.window())
            .ok()
            .and_then(|(_, _, _, primary, _)| primary)
            .is_none_or(|primary| primary.entity() != *root);
        (secondary, root.to_bits())
    });
    for (root, projection, presentation) in ordered_roots {
        let Ok((_, session)) = sessions.get(presentation.session) else {
            commands.entity(root).despawn();
            continue;
        };
        let Ok((_, _, intersections, primary, owner)) = windows.get(projection.window()) else {
            commands.entity(root).despawn();
            continue;
        };
        let authoritative = primary.is_some_and(|primary| primary.entity() == root);
        if projection.window() != session.source
            || session.source_mode != HoistSourceMode::PreservedSlot
            || presentation.state != placeholder_state(session.phase)
            || owner.owner() != presentation.session
            || (!authoritative && !intersections.contains(projection.output()))
            || !retained.insert((projection.window(), projection.output()))
        {
            commands.entity(root).despawn();
        }
    }

    for (session_entity, session) in &sessions {
        if session.source_mode != HoistSourceMode::PreservedSlot {
            continue;
        }
        let Ok((z_order, _, intersections, _, owner)) = windows.get(session.source) else {
            continue;
        };
        if owner.owner() != session_entity {
            continue;
        }
        for output in intersections.iter() {
            if !retained.insert((session.source, output)) {
                continue;
            }
            spawn_placeholder(
                &mut commands,
                session_entity,
                session,
                output,
                z_order.0,
                false,
                &outputs,
            );
        }
    }
}

fn spawn_placeholder(
    commands: &mut Commands,
    session_entity: Entity,
    session: &HoistSession,
    output: Entity,
    z_order: i32,
    primary: bool,
    outputs: &OutputCameraQuery,
) {
    let metrics = session.placeholder_metrics;
    let state = placeholder_state(session.phase);
    let root = match state {
        HoistPlaceholderState::Live => commands.spawn_scene(placeholder_scene(session_entity)).id(),
        HoistPlaceholderState::Closed => commands
            .spawn_scene(closed_placeholder_scene(session_entity))
            .id(),
    };
    let mut root_commands = commands.entity(root);
    root_commands.insert((
        WindowProjection::new(session.source, output),
        HoistPresentation {
            session: session_entity,
            state,
        },
        metrics.insets,
        metrics.offset,
        metrics.anchor,
        GlobalZIndex(z_order),
    ));
    if primary {
        root_commands.insert(PresentsWindow(session.source));
    }
    if let Ok((_, Some(camera), _)) = outputs.get(output)
        && let Some(camera) = camera.entity()
    {
        root_commands.insert(UiTargetCamera(camera));
    }
}

fn placeholder_state(phase: HoistSessionPhase) -> HoistPlaceholderState {
    if matches!(phase, HoistSessionPhase::Closed) {
        HoistPlaceholderState::Closed
    } else {
        HoistPlaceholderState::Live
    }
}

fn placeholder_scene(session: Entity) -> impl Scene {
    bsn! {
        Node {
            position_type: PositionType::Absolute,
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            overflow: Overflow::clip(),
        }
        BackgroundColor(Color::srgb(0.08, 0.10, 0.14))
        Children [
            (
                Pickable::IGNORE
                bevy::ui::widget::Text("Window hoisted")
                TextFont {
                    font: FontSourceTemplate::Monospace,
                    font_size: px(20.0),
                }
                bevy::text::TextColor(Color::srgb(0.88, 0.91, 0.96))
            ),
            (
                Button
                AccessibleLabel::new("Reclaim hoisted window")
                template(move |_| Ok(ReclaimHoistHandle(session)))
                Node {
                    width: px(132),
                    height: px(38),
                    margin: bevy::ui::UiRect::top(px(16)),
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    border_radius: BorderRadius::all(px(8)),
                }
                BackgroundColor(Color::srgb(0.20, 0.45, 0.72))
                Children [(
                    Pickable::IGNORE
                    bevy::ui::widget::Text("Reclaim")
                    TextFont {
                        font: FontSourceTemplate::Monospace,
                        font_size: px(16.0),
                    }
                    bevy::text::TextColor(Color::WHITE)
                )]
            ),
        ]
    }
}

fn closed_placeholder_scene(session: Entity) -> impl Scene {
    bsn! {
        Node {
            position_type: PositionType::Absolute,
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            overflow: Overflow::clip(),
        }
        BackgroundColor(Color::srgb(0.08, 0.10, 0.14))
        Children [
            (
                Pickable::IGNORE
                bevy::ui::widget::Text("Window closed remotely")
                TextFont {
                    font: FontSourceTemplate::Monospace,
                    font_size: px(20.0),
                }
                bevy::text::TextColor(Color::srgb(0.88, 0.91, 0.96))
            ),
            (
                Button
                AccessibleLabel::new("Dismiss closed remote window placeholder")
                template(move |_| Ok(DismissHoistTombstoneHandle(session)))
                Node {
                    width: px(132),
                    height: px(38),
                    margin: bevy::ui::UiRect::top(px(16)),
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    border_radius: BorderRadius::all(px(8)),
                }
                BackgroundColor(Color::srgb(0.32, 0.35, 0.42))
                Children [(
                    Pickable::IGNORE
                    bevy::ui::widget::Text("Dismiss")
                    TextFont {
                        font: FontSourceTemplate::Monospace,
                        font_size: px(16.0),
                    }
                    bevy::text::TextColor(Color::WHITE)
                )]
            ),
        ]
    }
}

pub(super) fn sync_hoist_root_sizes(
    windows: Query<&WindowGeometry>,
    mut roots: Query<(&WindowProjection, &mut Node), With<HoistPresentation>>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let mut changed = false;
    for (projection, mut node) in &mut roots {
        let Ok(geometry) = windows.get(projection.window()) else {
            continue;
        };
        let width = px(geometry.size.x.max(1.0));
        let height = px(geometry.size.y.max(1.0));
        if node.width != width || node.height != height {
            node.width = width;
            node.height = height;
            changed = true;
        }
    }
    if changed {
        redraw.write(RequestRedraw);
    }
}
