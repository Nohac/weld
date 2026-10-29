//! Lifetime-owned requests for attention, shared by applications and shell workflows.

use crate::{FocusedWindow, ManagedWindow, OccupiesWindow, WindowFocusChanged, WindowSystems};
use bevy::{
    app::{App, PreUpdate},
    ecs::{
        component::Component,
        entity::Entity,
        lifecycle::{Insert, Remove},
        message::MessageWriter,
        observer::On,
        query::{Changed, Or, With},
        schedule::IntoScheduleConfigs,
        system::{Commands, Query, Res},
    },
    window::RequestRedraw,
};
use weld_app::surface::ClientToplevelHints;

/// Attach to the entity owning an attention request; remove it when the request ends.
/// Windows can own shell requests themselves; applications use their client entity.
#[derive(Component, Clone, Copy, Debug)]
#[relationship(relationship_target = WindowAttention, allow_self_referential)]
pub struct AttentionForWindow(pub Entity);

/// Inverse membership of live attention requests for this window.
#[derive(Component, Debug)]
#[relationship_target(relationship = AttentionForWindow)]
pub struct WindowAttention(Vec<Entity>);

impl WindowAttention {
    pub fn is_urgent(&self) -> bool {
        !self.0.is_empty()
    }
}

#[derive(Component)]
struct ApplicationAttention {
    window: Entity,
    hint: bool,
}

pub(super) fn install(app: &mut App) {
    app.add_observer(acknowledge_focus)
        .add_observer(detach_client)
        .add_observer(attention_changed)
        .add_observer(attention_removed)
        .add_systems(
            PreUpdate,
            application_hints
                .after(WindowSystems::AdmissionPolicy)
                .before(WindowSystems::UiReconcile),
        );
}

type ChangedApplicationHints<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static ClientToplevelHints,
        &'static OccupiesWindow,
        Option<&'static ApplicationAttention>,
    ),
    Or<(Changed<ClientToplevelHints>, Changed<OccupiesWindow>)>,
>;

fn application_hints(
    clients: ChangedApplicationHints,
    windows: Query<(), With<ManagedWindow>>,
    focus: Res<FocusedWindow>,
    mut commands: Commands,
) {
    for (client, hints, occupant, previous) in &clients {
        if previous.is_some_and(|previous| {
            previous.window == occupant.0 && previous.hint == hints.0.urgent
        }) {
            continue;
        }
        commands.entity(client).insert(ApplicationAttention {
            window: occupant.0,
            hint: hints.0.urgent,
        });
        if hints.0.urgent && windows.contains(occupant.0) && focus.entity() != Some(occupant.0) {
            commands
                .entity(client)
                .insert(AttentionForWindow(occupant.0));
        } else {
            commands.entity(client).remove::<AttentionForWindow>();
        }
    }
}

fn acknowledge_focus(
    event: On<WindowFocusChanged>,
    clients: Query<(Entity, &AttentionForWindow), With<ApplicationAttention>>,
    mut commands: Commands,
) {
    for (client, claim) in &clients {
        if Some(claim.0) == event.window {
            commands.entity(client).remove::<AttentionForWindow>();
        }
    }
}

fn detach_client(
    event: On<Remove<OccupiesWindow>>,
    clients: Query<(), With<ApplicationAttention>>,
    mut commands: Commands,
) {
    if clients.contains(event.entity) {
        commands
            .entity(event.entity)
            .try_remove::<(ApplicationAttention, AttentionForWindow)>();
    }
}

fn attention_changed(_: On<Insert<AttentionForWindow>>, mut redraw: MessageWriter<RequestRedraw>) {
    redraw.write(RequestRedraw);
}
fn attention_removed(_: On<Remove<AttentionForWindow>>, mut redraw: MessageWriter<RequestRedraw>) {
    redraw.write(RequestRedraw);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WindowCommand, WindowCommandKind, WindowId, WindowPlugin, WindowVacancy};
    use weld_app::surface::SurfaceActionQueue;
    use weld_client::ToplevelHints;

    fn urgent(app: &App, window: Entity) -> bool {
        app.world()
            .get::<WindowAttention>(window)
            .is_some_and(WindowAttention::is_urgent)
    }

    #[test]
    fn focus_acknowledges_application_hints_without_retiring_other_attention_owners() {
        let mut app = App::new();
        app.init_resource::<SurfaceActionQueue>()
            .add_plugins(WindowPlugin);
        let window = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(1),
                },
                WindowVacancy::Retain,
            ))
            .id();
        let client = app
            .world_mut()
            .spawn((
                OccupiesWindow(window),
                ClientToplevelHints(ToplevelHints {
                    urgent: true,
                    ..Default::default()
                }),
            ))
            .id();
        app.update();
        assert!(urgent(&app, window));
        let request = app.world_mut().spawn(AttentionForWindow(window)).id();
        app.world_mut().trigger(WindowCommand {
            window,
            kind: WindowCommandKind::Focus,
        });
        app.update();
        assert!(app.world().get::<AttentionForWindow>(client).is_none());
        assert!(
            urgent(&app, window),
            "independent request survives local focus"
        );
        app.world_mut().despawn(request);
        app.update();
        assert!(!urgent(&app, window));
        app.world_mut().trigger(WindowCommand {
            window,
            kind: WindowCommandKind::ClearFocus,
        });
        app.world_mut()
            .get_mut::<ClientToplevelHints>(client)
            .expect("hints")
            .0
            .min_size
            .width = 20;
        app.update();
        assert!(
            !urgent(&app, window),
            "unrelated hint changes do not reassert acknowledged urgency"
        );
        app.world_mut()
            .get_mut::<ClientToplevelHints>(client)
            .expect("hints")
            .0
            .urgent = false;
        app.update();
        app.world_mut()
            .get_mut::<ClientToplevelHints>(client)
            .expect("hints")
            .0
            .urgent = true;
        app.update();
        assert!(urgent(&app, window));
        app.world_mut()
            .entity_mut(client)
            .remove::<OccupiesWindow>();
        app.update();
        assert!(
            !urgent(&app, window),
            "detaching the occupant releases its claim"
        );
    }
}
