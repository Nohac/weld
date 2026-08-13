//! Reusable unstyled pointer behavior for managed-window presentations.

use bevy::{
    ecs::{
        component::Component,
        hierarchy::ChildOf,
        observer::On,
        system::{Commands, Query, Res},
    },
    picking::{
        events::{Cancel, Drag, DragEnd, Pointer, Press},
        pointer::PointerButton,
    },
    ui::UiScale,
};
use weld_window::{
    PresentsWindow, WindowCommand, WindowCommandKind, WindowIntent, WindowIntentKind,
    WindowInteractionKind, WindowInteractionPhase, WindowInteractionSession,
};

/// Marks a UI entity whose primary-button drag moves its managed window.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct WindowMoveHandle;

pub(crate) fn activate_window(
    mut press: On<Pointer<Press>>,
    mut commands: Commands,
    presentations: Query<&PresentsWindow>,
    parents: Query<&ChildOf>,
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
}

pub(crate) fn begin_move_handle(
    press: On<Pointer<Press>>,
    mut commands: Commands,
    handles: Query<(), bevy::ecs::query::With<WindowMoveHandle>>,
    presentations: Query<&PresentsWindow>,
    parents: Query<&ChildOf>,
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
    commands.trigger(WindowCommand {
        window,
        kind: WindowCommandKind::BeginInteraction(WindowInteractionKind::Move),
    });
}

pub(crate) fn drag_window(
    mut drag: On<Pointer<Drag>>,
    mut commands: Commands,
    ui_scale: Res<UiScale>,
    presentations: Query<&PresentsWindow>,
    parents: Query<&ChildOf>,
    interactions: Query<&WindowInteractionSession>,
    handles: Query<(), bevy::ecs::query::With<WindowMoveHandle>>,
) {
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

#[cfg(test)]
mod tests {
    use bevy::{
        app::App,
        camera::{ManualTextureViewHandle, NormalizedRenderTarget},
        ecs::{hierarchy::ChildOf, observer::On, resource::Resource, system::ResMut},
        math::Vec2,
        picking::{
            events::{Drag, Pointer},
            pointer::{Location, PointerButton, PointerId},
        },
        ui::UiScale,
    };
    use weld_window::{
        PresentsWindow, WindowCommand, WindowCommandKind, WindowIntent, WindowIntentKind,
        WindowInteractionKind, WindowInteractionPhase, WindowInteractionSession,
    };

    use super::{WindowMoveHandle, drag_window, logical_drag_delta};

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

    #[test]
    fn direct_handle_drag_starts_only_when_no_interaction_session_exists() {
        let mut app = App::new();
        app.insert_resource(UiScale(1.0))
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
    }
}
