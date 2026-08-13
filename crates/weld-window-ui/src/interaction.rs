//! Reusable unstyled pointer behavior for managed-window presentations.

use bevy::{
    ecs::{
        component::Component,
        hierarchy::ChildOf,
        observer::On,
        system::{Commands, Query, Res, SystemParam},
    },
    picking::{
        events::{Cancel, Drag, DragEnd, Pointer, Press},
        pointer::PointerButton,
    },
    ui::{ComputedNode, UiScale},
    window::RequestRedraw,
};
use weld_app::surface::ToplevelResizeEdge;
use weld_window::{
    PresentsWindow, WindowCommand, WindowCommandKind, WindowIntent, WindowIntentKind,
    WindowInteractionKind, WindowInteractionPhase, WindowInteractionSession,
};

/// Marks a UI entity whose primary-button drag moves its managed window.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct WindowMoveHandle;

/// Marks a UI entity that starts resizing from one declared edge or corner.
#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowResizeHandle(pub ToplevelResizeEdge);

/// Marks a presentation root whose exposed border starts window resizing.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct WindowResizeFrame {
    /// Length of the corner target measured along each border edge, in logical pixels.
    pub corner_grab_extent: f32,
}

impl WindowResizeFrame {
    pub const fn new(corner_grab_extent: f32) -> Self {
        Self { corner_grab_extent }
    }
}

pub(crate) fn activate_window(
    mut press: On<Pointer<Press>>,
    mut commands: Commands,
    presentations: Query<&PresentsWindow>,
    parents: Query<&ChildOf>,
    mut redraw: bevy::ecs::message::MessageWriter<RequestRedraw>,
) {
    if press.button != PointerButton::Primary {
        return;
    }
    let Some(window) = presented_window(press.entity, &presentations, &parents) else {
        return;
    };
    press.propagate(false);
    commands.trigger(WindowIntent {
        window,
        kind: WindowIntentKind::Activate,
    });
    redraw.write(RequestRedraw);
}

pub(crate) fn begin_move_handle(
    press: On<Pointer<Press>>,
    mut commands: Commands,
    handles: Query<(), bevy::ecs::query::With<WindowMoveHandle>>,
    presentations: Query<&PresentsWindow>,
    parents: Query<&ChildOf>,
    interactions: Query<&WindowInteractionSession>,
) {
    if press.button != PointerButton::Primary
        || !handles.contains(press.entity)
        || press.original_event_target() != press.entity
    {
        return;
    }
    let Some(window) = presented_window(press.entity, &presentations, &parents) else {
        return;
    };
    if interactions.contains(window) {
        return;
    }
    commands.trigger(WindowCommand {
        window,
        kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Move),
    });
}

pub(crate) fn begin_resize_frame(
    press: On<Pointer<Press>>,
    mut commands: Commands,
    frames: Query<(&WindowResizeFrame, &ComputedNode)>,
    presentations: Query<&PresentsWindow>,
    interactions: Query<&WindowInteractionSession>,
) {
    if press.button != PointerButton::Primary || press.original_event_target() != press.entity {
        return;
    }
    let Ok((frame, computed)) = frames.get(press.entity) else {
        return;
    };
    let Ok(presentation) = presentations.get(press.entity) else {
        return;
    };
    if interactions.contains(presentation.0) {
        return;
    }
    let Some(position) = press.hit.position.map(|position| position.truncate()) else {
        return;
    };
    let Some(edges) = resize_edge_at(*frame, computed, position) else {
        return;
    };
    commands.trigger(WindowCommand {
        window: presentation.0,
        kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Resize(edges)),
    });
}

pub(crate) fn begin_resize_handle(
    press: On<Pointer<Press>>,
    mut commands: Commands,
    handles: Query<&WindowResizeHandle>,
    presentations: Query<&PresentsWindow>,
    parents: Query<&ChildOf>,
    interactions: Query<&WindowInteractionSession>,
) {
    if press.button != PointerButton::Primary || press.original_event_target() != press.entity {
        return;
    }
    let Ok(handle) = handles.get(press.entity) else {
        return;
    };
    let Some(window) = presented_window(press.entity, &presentations, &parents) else {
        return;
    };
    if interactions.contains(window) {
        return;
    }
    commands.trigger(WindowCommand {
        window,
        kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Resize(handle.0)),
    });
}

#[derive(SystemParam)]
pub(crate) struct DragWindowParams<'w, 's> {
    commands: Commands<'w, 's>,
    ui_scale: Res<'w, UiScale>,
    presentations: Query<'w, 's, &'static PresentsWindow>,
    parents: Query<'w, 's, &'static ChildOf>,
    interactions: Query<'w, 's, &'static WindowInteractionSession>,
    handles: Query<'w, 's, (), bevy::ecs::query::With<WindowMoveHandle>>,
    redraw: bevy::ecs::message::MessageWriter<'w, RequestRedraw>,
}

pub(crate) fn drag_window(mut drag: On<Pointer<Drag>>, params: DragWindowParams) {
    let DragWindowParams {
        mut commands,
        ui_scale,
        presentations,
        parents,
        interactions,
        handles,
        mut redraw,
    } = params;
    if drag.button != PointerButton::Primary {
        return;
    }
    let Some(window) = presented_window(drag.entity, &presentations, &parents) else {
        return;
    };
    let Some(delta) = logical_drag_delta(drag.delta, ui_scale.0) else {
        return;
    };
    let kind = match interactions.get(window) {
        Ok(interaction) if interaction.phase == WindowInteractionPhase::Active => {
            match interaction.kind {
                WindowInteractionKind::Move => WindowIntentKind::MoveBy(delta),
                WindowInteractionKind::Resize(_) => WindowIntentKind::ResizeBy(delta),
            }
        }
        Err(_) if handles.contains(drag.entity) && drag.original_event_target() == drag.entity => {
            commands.trigger(WindowCommand {
                window,
                kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Move),
            });
            WindowIntentKind::MoveBy(delta)
        }
        _ => return,
    };
    drag.propagate(false);
    commands.trigger(WindowIntent { window, kind });
    redraw.write(RequestRedraw);
}

pub(crate) fn end_drag(
    drag_end: On<Pointer<DragEnd>>,
    mut commands: Commands,
    presentations: Query<&PresentsWindow>,
    parents: Query<&ChildOf>,
    interactions: Query<&WindowInteractionSession>,
) {
    if drag_end.button != PointerButton::Primary {
        return;
    }
    let Some(window) = presented_window(drag_end.entity, &presentations, &parents) else {
        return;
    };
    if interactions.get(window).is_ok() {
        commands.trigger(WindowIntent {
            window,
            kind: WindowIntentKind::InteractionEnded,
        });
    }
}

pub(crate) fn cancel_drag(
    cancel: On<Pointer<Cancel>>,
    mut commands: Commands,
    presentations: Query<&PresentsWindow>,
    parents: Query<&ChildOf>,
    interactions: Query<&WindowInteractionSession>,
) {
    let Some(window) = presented_window(cancel.entity, &presentations, &parents) else {
        return;
    };
    if interactions.get(window).is_ok() {
        commands.trigger(WindowIntent {
            window,
            kind: WindowIntentKind::InteractionEnded,
        });
    }
}

fn presented_window(
    mut entity: bevy::ecs::entity::Entity,
    presentations: &Query<&PresentsWindow>,
    parents: &Query<&ChildOf>,
) -> Option<bevy::ecs::entity::Entity> {
    loop {
        if let Ok(presentation) = presentations.get(entity) {
            return Some(presentation.0);
        }
        entity = parents.get(entity).ok()?.parent();
    }
}

fn logical_drag_delta(delta: bevy::math::Vec2, scale: f32) -> Option<bevy::math::Vec2> {
    (scale.is_finite() && scale > 0.0).then_some(delta / scale)
}

fn resize_edge_at(
    frame: WindowResizeFrame,
    computed: &ComputedNode,
    normalized_position: bevy::math::Vec2,
) -> Option<ToplevelResizeEdge> {
    let size = computed.size();
    let inverse_scale = computed.inverse_scale_factor();
    if !size.is_finite()
        || size.cmple(bevy::math::Vec2::ZERO).any()
        || !normalized_position.is_finite()
        || normalized_position
            .cmplt(bevy::math::Vec2::splat(-0.5))
            .any()
        || normalized_position
            .cmpgt(bevy::math::Vec2::splat(0.5))
            .any()
        || !inverse_scale.is_finite()
        || inverse_scale <= 0.0
        || !frame.corner_grab_extent.is_finite()
        || frame.corner_grab_extent < 0.0
    {
        return None;
    }

    let border = computed.border;
    if !border.min_inset.is_finite()
        || !border.max_inset.is_finite()
        || (border.min_inset.max(border.max_inset)).max_element() <= 0.0
    {
        return None;
    }
    let from_min = (normalized_position + bevy::math::Vec2::splat(0.5)) * size;
    let from_max = size - from_min;
    let on_left = from_min.x <= border.min_inset.x;
    let on_top = from_min.y <= border.min_inset.y;
    let on_right = from_max.x <= border.max_inset.x;
    let on_bottom = from_max.y <= border.max_inset.y;
    if !(on_left || on_top || on_right || on_bottom) {
        return None;
    }

    // Computed sizes and borders are physical pixels; inverse_scale_factor is
    // logical pixels per physical pixel.
    let corner_extent = frame.corner_grab_extent / inverse_scale;
    let near_left = from_min.x <= corner_extent;
    let near_top = from_min.y <= corner_extent;
    let near_right = from_max.x <= corner_extent;
    let near_bottom = from_max.y <= corner_extent;

    if (on_top && near_left) || (on_left && near_top) {
        Some(ToplevelResizeEdge::TopLeft)
    } else if (on_top && near_right) || (on_right && near_top) {
        Some(ToplevelResizeEdge::TopRight)
    } else if (on_bottom && near_left) || (on_left && near_bottom) {
        Some(ToplevelResizeEdge::BottomLeft)
    } else if (on_bottom && near_right) || (on_right && near_bottom) {
        Some(ToplevelResizeEdge::BottomRight)
    } else if on_left {
        Some(ToplevelResizeEdge::Left)
    } else if on_top {
        Some(ToplevelResizeEdge::Top)
    } else if on_right {
        Some(ToplevelResizeEdge::Right)
    } else if on_bottom {
        Some(ToplevelResizeEdge::Bottom)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use bevy::{
        app::App,
        camera::{ManualTextureViewHandle, NormalizedRenderTarget},
        ecs::{
            hierarchy::ChildOf, message::MessageCursor, observer::On, resource::Resource,
            system::ResMut,
        },
        math::{Vec2, Vec3},
        picking::{
            backend::HitData,
            events::{Drag, Pointer, Press},
            pointer::{Location, PointerButton, PointerId},
        },
        sprite::BorderRect,
        ui::{ComputedNode, UiScale},
        window::RequestRedraw,
    };
    use weld_app::surface::ToplevelResizeEdge;
    use weld_window::{
        PresentsWindow, WindowCommand, WindowCommandKind, WindowIntent, WindowIntentKind,
        WindowInteractionKind, WindowInteractionPhase, WindowInteractionSession,
    };

    use super::{
        WindowMoveHandle, WindowResizeFrame, WindowResizeHandle, activate_window,
        begin_resize_frame, begin_resize_handle, drag_window, logical_drag_delta, resize_edge_at,
    };

    #[derive(Resource, Default)]
    struct RecordedInteractions {
        commands: Vec<WindowCommandKind>,
        intents: Vec<WindowIntentKind>,
    }

    fn record_command(command: On<WindowCommand>, mut recorded: ResMut<RecordedInteractions>) {
        recorded.commands.push(command.kind);
    }

    fn record_intent(intent: On<WindowIntent>, mut recorded: ResMut<RecordedInteractions>) {
        recorded.intents.push(intent.kind);
    }

    #[test]
    fn drag_delta_converts_physical_ui_motion_to_logical_units() {
        assert_eq!(
            logical_drag_delta(Vec2::new(18.0, 12.0), 1.5),
            Some(Vec2::new(12.0, 8.0))
        );
        assert_eq!(logical_drag_delta(Vec2::ONE, 0.0), None);
        assert_eq!(logical_drag_delta(Vec2::ONE, f32::NAN), None);
    }

    fn resize_node(inverse_scale_factor: f32) -> ComputedNode {
        ComputedNode {
            size: Vec2::new(200.0, 100.0),
            border: BorderRect::all(3.0),
            inverse_scale_factor,
            ..Default::default()
        }
    }

    #[test]
    fn resize_frame_classifies_edges_corners_and_rejects_the_interior() {
        let frame = WindowResizeFrame::new(12.0);
        let node = resize_node(1.0);
        for (position, expected) in [
            (Vec2::new(-0.49, 0.0), ToplevelResizeEdge::Left),
            (Vec2::new(0.49, 0.0), ToplevelResizeEdge::Right),
            (Vec2::new(0.0, -0.49), ToplevelResizeEdge::Top),
            (Vec2::new(0.0, 0.49), ToplevelResizeEdge::Bottom),
            (Vec2::new(-0.45, -0.49), ToplevelResizeEdge::TopLeft),
            (Vec2::new(0.45, -0.49), ToplevelResizeEdge::TopRight),
            (Vec2::new(-0.45, 0.49), ToplevelResizeEdge::BottomLeft),
            (Vec2::new(0.45, 0.49), ToplevelResizeEdge::BottomRight),
        ] {
            assert_eq!(resize_edge_at(frame, &node, position), Some(expected));
        }
        assert_eq!(resize_edge_at(frame, &node, Vec2::ZERO), None);
        assert_eq!(
            resize_edge_at(frame, &ComputedNode::default(), Vec2::new(-0.5, 0.0)),
            None
        );

        let scaled = resize_node(0.5);
        assert_eq!(
            resize_edge_at(frame, &scaled, Vec2::new(-0.40, -0.49)),
            Some(ToplevelResizeEdge::TopLeft)
        );
    }

    #[test]
    fn direct_border_press_begins_resize_but_existing_sessions_are_preserved() {
        let mut app = App::new();
        app.init_resource::<RecordedInteractions>()
            .add_observer(begin_resize_frame)
            .add_observer(record_command);
        let window = app.world_mut().spawn_empty().id();
        let root = app
            .world_mut()
            .spawn((
                PresentsWindow(window),
                WindowResizeFrame::new(12.0),
                resize_node(1.0),
            ))
            .id();
        let camera = app.world_mut().spawn_empty().id();
        let press = || {
            Pointer::new(
                PointerId::Mouse,
                Location {
                    target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
                    position: Vec2::ZERO,
                },
                Press {
                    button: PointerButton::Primary,
                    hit: HitData::new(camera, 0.0, Some(Vec3::new(-0.49, 0.0, 0.0)), None),
                    count: 1,
                },
                root,
            )
        };

        app.world_mut().trigger(press());
        app.update();
        assert_eq!(
            app.world().resource::<RecordedInteractions>().commands,
            vec![WindowCommandKind::BeginInteraction(
                WindowInteractionKind::Resize(ToplevelResizeEdge::Left)
            )]
        );

        app.world_mut()
            .resource_mut::<RecordedInteractions>()
            .commands
            .clear();
        app.world_mut()
            .entity_mut(window)
            .insert(WindowInteractionSession {
                kind: WindowInteractionKind::Resize(ToplevelResizeEdge::Left),
                phase: WindowInteractionPhase::Ending,
            });
        app.world_mut().trigger(press());
        app.update();
        assert!(
            app.world()
                .resource::<RecordedInteractions>()
                .commands
                .is_empty()
        );
    }

    #[test]
    fn direct_resize_handles_emit_their_declared_edges_only() {
        let mut app = App::new();
        app.init_resource::<RecordedInteractions>()
            .add_observer(begin_resize_handle)
            .add_observer(record_command);
        let window = app.world_mut().spawn_empty().id();
        let root = app.world_mut().spawn(PresentsWindow(window)).id();
        let camera = app.world_mut().spawn_empty().id();
        let location = Location {
            target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
            position: Vec2::ZERO,
        };
        let press = |target, button| {
            Pointer::new(
                PointerId::Mouse,
                location.clone(),
                Press {
                    button,
                    hit: HitData::new(camera, 0.0, None, None),
                    count: 1,
                },
                target,
            )
        };

        for edge in [
            ToplevelResizeEdge::Top,
            ToplevelResizeEdge::Bottom,
            ToplevelResizeEdge::Left,
            ToplevelResizeEdge::Right,
            ToplevelResizeEdge::TopLeft,
            ToplevelResizeEdge::BottomLeft,
            ToplevelResizeEdge::TopRight,
            ToplevelResizeEdge::BottomRight,
        ] {
            let handle = app
                .world_mut()
                .spawn((WindowResizeHandle(edge), ChildOf(root)))
                .id();
            app.world_mut()
                .trigger(press(handle, PointerButton::Primary));
            app.update();
            assert_eq!(
                app.world().resource::<RecordedInteractions>().commands,
                [WindowCommandKind::BeginInteraction(
                    WindowInteractionKind::Resize(edge)
                )]
            );
            app.world_mut()
                .resource_mut::<RecordedInteractions>()
                .commands
                .clear();
        }

        let handle = app
            .world_mut()
            .spawn((WindowResizeHandle(ToplevelResizeEdge::Left), ChildOf(root)))
            .id();
        app.world_mut()
            .trigger(press(handle, PointerButton::Secondary));
        app.update();
        assert!(
            app.world()
                .resource::<RecordedInteractions>()
                .commands
                .is_empty()
        );

        let child = app.world_mut().spawn(ChildOf(handle)).id();
        app.world_mut()
            .trigger(press(child, PointerButton::Primary));
        app.update();
        assert!(
            app.world()
                .resource::<RecordedInteractions>()
                .commands
                .is_empty()
        );

        app.world_mut()
            .entity_mut(window)
            .insert(WindowInteractionSession {
                kind: WindowInteractionKind::Resize(ToplevelResizeEdge::Left),
                phase: WindowInteractionPhase::Ending,
            });
        app.world_mut()
            .trigger(press(handle, PointerButton::Primary));
        app.update();
        assert!(
            app.world()
                .resource::<RecordedInteractions>()
                .commands
                .is_empty()
        );
    }

    #[test]
    fn activating_a_presented_window_requests_a_redraw() {
        let mut app = App::new();
        app.add_message::<RequestRedraw>()
            .init_resource::<RecordedInteractions>()
            .add_observer(activate_window)
            .add_observer(record_intent);
        let window = app.world_mut().spawn_empty().id();
        let root = app.world_mut().spawn(PresentsWindow(window)).id();
        let camera = app.world_mut().spawn_empty().id();
        let mut redraws = MessageCursor::<RequestRedraw>::default();

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
                position: Vec2::ZERO,
            },
            Press {
                button: PointerButton::Primary,
                hit: HitData::new(camera, 0.0, None, None),
                count: 1,
            },
            root,
        ));
        app.update();

        assert_eq!(
            app.world().resource::<RecordedInteractions>().intents,
            vec![WindowIntentKind::Activate]
        );
        assert_eq!(
            redraws
                .read(
                    app.world()
                        .resource::<bevy::ecs::message::Messages<RequestRedraw>>()
                )
                .count(),
            1
        );
    }

    #[test]
    fn direct_handle_drag_starts_only_when_no_interaction_session_exists() {
        let mut app = App::new();
        app.insert_resource(UiScale(1.0))
            .add_message::<RequestRedraw>()
            .init_resource::<RecordedInteractions>()
            .add_observer(drag_window)
            .add_observer(record_command)
            .add_observer(record_intent);
        let window = app.world_mut().spawn_empty().id();
        let root = app.world_mut().spawn(PresentsWindow(window)).id();
        let handle = app
            .world_mut()
            .spawn((WindowMoveHandle, ChildOf(root)))
            .id();
        let location = Location {
            target: NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1)),
            position: Vec2::ZERO,
        };
        let mut redraws = MessageCursor::<RequestRedraw>::default();
        let drag = || {
            Pointer::new(
                PointerId::Mouse,
                location.clone(),
                Drag {
                    button: PointerButton::Primary,
                    distance: Vec2::new(12.0, 8.0),
                    delta: Vec2::new(12.0, 8.0),
                },
                handle,
            )
        };

        app.world_mut().trigger(drag());
        app.update();
        assert_eq!(
            app.world().resource::<RecordedInteractions>().commands,
            vec![WindowCommandKind::BeginInteraction(
                WindowInteractionKind::Move
            )]
        );
        assert_eq!(
            app.world().resource::<RecordedInteractions>().intents,
            vec![WindowIntentKind::MoveBy(Vec2::new(12.0, 8.0))]
        );
        assert_eq!(
            redraws
                .read(
                    app.world()
                        .resource::<bevy::ecs::message::Messages<RequestRedraw>>()
                )
                .count(),
            1
        );

        app.world_mut()
            .resource_mut::<RecordedInteractions>()
            .commands
            .clear();
        app.world_mut()
            .resource_mut::<RecordedInteractions>()
            .intents
            .clear();
        app.world_mut()
            .entity_mut(window)
            .insert(WindowInteractionSession {
                kind: WindowInteractionKind::Resize(weld_app::surface::ToplevelResizeEdge::TopLeft),
                phase: WindowInteractionPhase::Ending,
            });
        app.world_mut().trigger(drag());
        app.update();

        let recorded = app.world().resource::<RecordedInteractions>();
        assert!(recorded.commands.is_empty());
        assert!(recorded.intents.is_empty());
        assert_eq!(
            redraws
                .read(
                    app.world()
                        .resource::<bevy::ecs::message::Messages<RequestRedraw>>()
                )
                .count(),
            0
        );
    }
}
