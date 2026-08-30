//! Hoist placeholder, reclaim, and tombstone presentation.

use std::collections::HashSet;

use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        component::Component,
        entity::Entity,
        message::{Message, MessageWriter},
        observer::On,
        query::With,
        schedule::IntoScheduleConfigs,
        system::{Commands, Query},
        template::template,
    },
    picking::{
        Pickable,
        events::{Click, Pointer},
        pointer::PointerButton,
    },
    prelude::{
        AccessibleLabel, AlignItems, BackgroundColor, BorderColor, BorderRadius, BoxShadow, Button,
        Children, Color, FlexDirection, GlobalZIndex, JustifyContent, Node, Overflow, PositionType,
        Scene, UiRect, UiTargetCamera, px,
    },
    scene::{CommandsSceneExt, bsn},
    text::{FontSourceTemplate, TextFont},
    window::RequestRedraw,
};
use weld_app::output::{OutputCompositionCamera, PrimaryOutput, WeldOutput};
use weld_window::{
    ManagedWindow, PresentationInsets, PresentationOffset, PresentsWindow,
    PrimaryWindowPresentation, WindowGeometryAnchor, WindowOutput, WindowOutputIntersections,
    WindowPresentationOverride, WindowProjection, WindowSystems, WindowZOrder,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct HoistPlaceholderMetrics {
    pub insets: PresentationInsets,
    pub offset: PresentationOffset,
    pub anchor: WindowGeometryAnchor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoistPlaceholderState {
    Live,
    Closed,
}

#[derive(Component, Clone, Copy, Debug)]
pub struct HoistPlaceholder {
    pub session: Entity,
    pub state: HoistPlaceholderState,
    pub metrics: HoistPlaceholderMetrics,
}

#[derive(Clone, Copy, Debug, Message)]
pub struct ReclaimHoist {
    pub session: Entity,
}

#[derive(Clone, Copy, Debug, Message)]
pub struct DismissHoistTombstone {
    pub session: Entity,
}

#[derive(Component, Clone, Copy, Debug)]
struct HoistActionHandle {
    session: Entity,
    reclaim: bool,
}

#[derive(Component, Clone, Copy, Debug)]
pub struct HoistPresentation {
    pub session: Entity,
    pub state: HoistPlaceholderState,
}

/// Hoist placeholder presentation for applications that also install the
/// generic Weld window-UI projection plugin.
pub struct HoistUiPlugin;

impl Plugin for HoistUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<ReclaimHoist>()
            .add_message::<DismissHoistTombstone>()
            .add_observer(request_action)
            .add_systems(
                PreUpdate,
                revoke_presentations.in_set(WindowSystems::PresentationRevoke),
            )
            .add_systems(
                PreUpdate,
                present_placeholders.in_set(WindowSystems::PresentationClaim),
            )
            .add_systems(
                PreUpdate,
                reconcile_projections.in_set(WindowSystems::UiReconcile),
            );
    }
}

fn request_action(
    mut click: On<Pointer<Click>>,
    handles: Query<&HoistActionHandle>,
    mut reclaims: MessageWriter<ReclaimHoist>,
    mut dismissals: MessageWriter<DismissHoistTombstone>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if click.button != PointerButton::Primary || click.original_event_target() != click.entity {
        return;
    }
    let Ok(handle) = handles.get(click.entity) else {
        return;
    };
    click.propagate(false);
    if handle.reclaim {
        reclaims.write(ReclaimHoist {
            session: handle.session,
        });
    } else {
        dismissals.write(DismissHoistTombstone {
            session: handle.session,
        });
    }
    redraw.write(RequestRedraw);
}

fn revoke_presentations(
    mut commands: Commands,
    roots: Query<(Entity, &WindowProjection, &HoistPresentation)>,
    windows: Query<(&HoistPlaceholder, &WindowPresentationOverride)>,
) {
    for (root, projection, presentation) in &roots {
        let valid = windows
            .get(projection.window())
            .is_ok_and(|(placeholder, owner)| {
                placeholder.session == presentation.session
                    && placeholder.state == presentation.state
                    && owner.owner() == placeholder.session
            });
        if !valid {
            commands.entity(root).despawn();
        }
    }
}

type PlaceholderWindows<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static HoistPlaceholder,
        &'static WindowZOrder,
        Option<&'static WindowOutput>,
        &'static WindowOutputIntersections,
        Option<&'static PrimaryWindowPresentation>,
        &'static WindowPresentationOverride,
    ),
    With<ManagedWindow>,
>;

type OutputCameras<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        Option<&'static OutputCompositionCamera>,
        Option<&'static PrimaryOutput>,
    ),
    With<WeldOutput>,
>;

fn present_placeholders(
    mut commands: Commands,
    windows: PlaceholderWindows,
    outputs: OutputCameras,
) {
    for (window, placeholder, z_order, output, _, primary, owner) in &windows {
        if primary.is_some() || owner.owner() != placeholder.session {
            continue;
        }
        let output = output.map(|output| output.0).or_else(|| {
            outputs
                .iter()
                .find_map(|(output, _, primary)| primary.is_some().then_some(output))
        });
        if let Some(output) = output {
            spawn_placeholder(
                &mut commands,
                window,
                *placeholder,
                output,
                z_order.0,
                true,
                &outputs,
            );
        }
    }
}

fn reconcile_projections(
    mut commands: Commands,
    windows: PlaceholderWindows,
    outputs: OutputCameras,
    roots: Query<(Entity, &WindowProjection, &HoistPresentation)>,
) {
    let mut retained = HashSet::new();
    for (root, projection, presentation) in &roots {
        let Ok((window, placeholder, _, _, intersections, primary, owner)) =
            windows.get(projection.window())
        else {
            commands.entity(root).despawn();
            continue;
        };
        let authoritative = primary.is_some_and(|primary| primary.entity() == root);
        if window != projection.window()
            || placeholder.session != presentation.session
            || placeholder.state != presentation.state
            || owner.owner() != placeholder.session
            || (!authoritative && !intersections.contains(projection.output()))
            || !retained.insert((window, projection.output()))
        {
            commands.entity(root).despawn();
        }
    }
    for (window, placeholder, z_order, _, intersections, _, owner) in &windows {
        if owner.owner() != placeholder.session {
            continue;
        }
        for output in intersections.iter() {
            if retained.insert((window, output)) {
                spawn_placeholder(
                    &mut commands,
                    window,
                    *placeholder,
                    output,
                    z_order.0,
                    false,
                    &outputs,
                );
            }
        }
    }
}

fn spawn_placeholder(
    commands: &mut Commands,
    window: Entity,
    placeholder: HoistPlaceholder,
    output: Entity,
    z_order: i32,
    primary: bool,
    outputs: &OutputCameras,
) {
    let root = match placeholder.state {
        HoistPlaceholderState::Live => commands.spawn_scene(live_scene(placeholder.session)).id(),
        HoistPlaceholderState::Closed => {
            commands.spawn_scene(closed_scene(placeholder.session)).id()
        }
    };
    let mut entity = commands.entity(root);
    entity.insert((
        WindowProjection::new(window, output),
        HoistPresentation {
            session: placeholder.session,
            state: placeholder.state,
        },
        placeholder.metrics.insets,
        placeholder.metrics.offset,
        placeholder.metrics.anchor,
        GlobalZIndex(z_order),
    ));
    if primary {
        entity.insert(PresentsWindow(window));
    }
    if let Ok((_, Some(camera), _)) = outputs.get(output)
        && let Some(camera) = camera.entity()
    {
        entity.insert(UiTargetCamera(camera));
    }
}

fn live_scene(session: Entity) -> impl Scene {
    placeholder_scene(
        session,
        "Window hoisted",
        "Reclaim hoisted window",
        "Reclaim",
        Color::srgb(0.20, 0.45, 0.72),
        true,
    )
}

fn closed_scene(session: Entity) -> impl Scene {
    placeholder_scene(
        session,
        "Window closed remotely",
        "Dismiss closed remote window placeholder",
        "Dismiss",
        Color::srgb(0.32, 0.35, 0.42),
        false,
    )
}

fn placeholder_scene(
    session: Entity,
    message: &'static str,
    label: &'static str,
    action: &'static str,
    color: Color,
    reclaim: bool,
) -> impl Scene {
    bsn! {
        Node {
            position_type: PositionType::Absolute,
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            overflow: Overflow::clip(),
            border: UiRect::all(px(2)),
            border_radius: BorderRadius::all(px(10)),
        }
        BackgroundColor(Color::srgb(0.08, 0.10, 0.14))
        BorderColor::all(Color::srgb(0.28, 0.34, 0.44))
        BoxShadow::new(
            Color::srgba(0.0, 0.0, 0.0, 0.55),
            px(0),
            px(10),
            px(2),
            px(22),
        )
        Children [
            (
                Pickable::IGNORE
                bevy::ui::widget::Text(message)
                TextFont { font: FontSourceTemplate::Monospace, font_size: px(20.0) }
                bevy::text::TextColor(Color::srgb(0.88, 0.91, 0.96))
            ),
            (
                Button
                AccessibleLabel::new(label)
                template(move |_| Ok(HoistActionHandle { session, reclaim }))
                Node {
                    width: px(132),
                    height: px(38),
                    margin: bevy::ui::UiRect::top(px(16)),
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    border_radius: BorderRadius::all(px(8)),
                }
                BackgroundColor(color)
                Children [(
                    Pickable::IGNORE
                    bevy::ui::widget::Text(action)
                    TextFont { font: FontSourceTemplate::Monospace, font_size: px(16.0) }
                    bevy::text::TextColor(Color::WHITE)
                )]
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use bevy::{
        app::App,
        camera::{ManualTextureViewHandle, NormalizedRenderTarget},
        ecs::message::Messages,
        math::Vec2,
        picking::{
            backend::HitData,
            events::{Click, Pointer},
            pointer::{Location, PointerButton, PointerId},
        },
    };

    use super::*;

    #[test]
    fn accepted_hoist_action_requests_redraw() {
        let mut app = App::new();
        app.add_message::<RequestRedraw>()
            .add_plugins(HoistUiPlugin);
        let session = app.world_mut().spawn_empty().id();
        let button = app
            .world_mut()
            .spawn(HoistActionHandle {
                session,
                reclaim: true,
            })
            .id();
        let camera = app.world_mut().spawn_empty().id();

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
                position: Vec2::ZERO,
            },
            Click {
                button: PointerButton::Primary,
                hit: HitData::new(camera, 0.0, None, None),
                duration: std::time::Duration::ZERO,
                count: 1,
            },
            button,
        ));

        assert_eq!(app.world().resource::<Messages<ReclaimHoist>>().len(), 1);
        assert_eq!(app.world().resource::<Messages<RequestRedraw>>().len(), 1);
    }
}
