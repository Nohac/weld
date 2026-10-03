//! Reversible output-sized presentation over retained window layouts.

use crate::{
    AppliedPresentationInsets, FocusedWindow, ManagedWindow, OccupiesWindow, WindowClientResolver,
    WindowCommand, WindowCommandKind, WindowGeometry, WindowOccupant, WindowOutput, WindowSystems,
    WindowVacancy, WindowVisibility, WindowZOrder, workspace::WorkspaceMember,
};
use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        component::Component,
        entity::Entity,
        event::Event,
        message::MessageWriter,
        observer::On,
        query::{QueryData, With},
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Commands, Query, Res, ResMut},
    },
    math::Vec2,
    window::RequestRedraw,
};
use std::collections::HashSet;
use weld_app::{
    layer_shell::DesktopLayerVisibility,
    output::{OutputGeometry, WeldOutput},
    surface::{ClientToplevelParent, PendingClientFullscreen},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FullscreenMode {
    /// Fill the workspace's output, retaining desktop overlay access.
    Normal,
    /// Give this application the output, including its ordinary overlay/input plane.
    /// Compositor escape shortcuts remain available.
    Exclusive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FullscreenAction {
    Enable(FullscreenMode),
    Toggle(FullscreenMode),
    Disable,
}

#[derive(Event, Clone, Copy, Debug)]
pub struct FullscreenRequest {
    /// Absence captures the management selection when the request is queued.
    pub window: Option<Entity>,
    pub action: FullscreenAction,
}

#[derive(Resource, Default)]
struct PendingRequests(Vec<ApplyRequest>);

/// Hold configure publication until the queued frame transition has applied.
#[derive(Component)]
pub(crate) struct PendingFullscreenConfigure;

#[derive(Event)]
struct ApplyRequest {
    window: Entity,
    action: FullscreenAction,
}

fn queue_request(
    event: On<FullscreenRequest>,
    focus: Res<FocusedWindow>,
    mut pending: ResMut<PendingRequests>,
    mut redraw: MessageWriter<RequestRedraw>,
    mut commands: Commands,
) {
    if let Some(window) = event.window.or(focus.entity()) {
        pending.0.push(ApplyRequest {
            window,
            action: event.action,
        });
        commands
            .entity(window)
            .try_insert(PendingFullscreenConfigure);
        redraw.write(RequestRedraw);
    }
}

fn apply_pending(mut pending: ResMut<PendingRequests>, mut commands: Commands) {
    for request in pending.0.drain(..) {
        commands
            .entity(request.window)
            .try_remove::<PendingFullscreenConfigure>();
        commands.trigger(request);
    }
}

/// Retained restore state; layout membership and client occupancy remain unchanged.
#[derive(Component, Clone, Copy, Debug)]
pub struct WindowFullscreen {
    pub mode: FullscreenMode,
    restore_geometry: WindowGeometry,
    restore_z: WindowZOrder,
    restore_insets: AppliedPresentationInsets,
}

/// A local presentation hidden by another window's fullscreen claim.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct FullscreenOccluded;

/// Active output claim, also used to suppress foreign cross-output projections.
#[derive(Component, Clone, Copy)]
pub struct FullscreenOutput {
    window: Entity,
    mode: FullscreenMode,
    previous_layers: Option<DesktopLayerVisibility>,
}

impl FullscreenOutput {
    pub fn allows_window(
        &self,
        window: Entity,
        clients: &WindowClientResolver,
        parents: &Query<&ClientToplevelParent>,
    ) -> bool {
        belongs_to_application(window, self.window, clients, parents)
    }
}

pub struct FullscreenPlugin;

impl Plugin for FullscreenPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PendingRequests>()
            .add_observer(queue_request)
            .add_observer(request)
            .add_systems(
                PreUpdate,
                apply_pending
                    .after(WindowSystems::Admission)
                    .before(WindowSystems::PresentationRevoke),
            )
            .add_systems(
                PreUpdate,
                protocol_requests
                    .after(WindowSystems::Management)
                    .before(reconcile),
            )
            .add_systems(
                PreUpdate,
                reconcile
                    .after(WindowSystems::Management)
                    .before(WindowSystems::OutputAssignment),
            );
    }
}

#[derive(QueryData)]
struct Target {
    entity: Entity,
    geometry: &'static WindowGeometry,
    z: &'static WindowZOrder,
    insets: &'static AppliedPresentationInsets,
    output: &'static WindowOutput,
    workspace: Option<&'static WorkspaceMember>,
    fullscreen: Option<&'static WindowFullscreen>,
    visibility: &'static WindowVisibility,
}

fn restore(commands: &mut Commands, window: Entity, state: WindowFullscreen) {
    commands
        .entity(window)
        .remove::<WindowFullscreen>()
        .insert((
            state.restore_geometry,
            state.restore_z,
            state.restore_insets,
        ));
    commands.trigger(WindowCommand {
        window,
        kind: WindowCommandKind::EndInteraction,
    });
}

fn request(
    event: On<ApplyRequest>,
    windows: Query<Target, With<ManagedWindow>>,
    mut commands: Commands,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let window = event.window;
    let Ok(target) = windows.get(window) else {
        return;
    };
    let mode = match event.action {
        FullscreenAction::Disable => None,
        FullscreenAction::Enable(mode) => Some(mode),
        FullscreenAction::Toggle(mode) => {
            if target
                .fullscreen
                .is_some_and(|state| mode == FullscreenMode::Normal || state.mode == mode)
            {
                None
            } else {
                Some(mode)
            }
        }
    };
    let Some(mode) = mode else {
        if let Some(state) = target.fullscreen {
            restore(&mut commands, window, *state);
            redraw.write(RequestRedraw);
        }
        return;
    };
    if target.fullscreen.is_some_and(|state| state.mode == mode) {
        return;
    }
    for other in &windows {
        if other.entity != window
            && other.output == target.output
            && other.workspace == target.workspace
            && let Some(state) = other.fullscreen
        {
            restore(&mut commands, other.entity, *state);
        }
    }
    let state = target.fullscreen.copied().map_or(
        WindowFullscreen {
            mode,
            restore_geometry: *target.geometry,
            restore_z: *target.z,
            restore_insets: *target.insets,
        },
        |old| WindowFullscreen { mode, ..old },
    );
    commands.trigger(WindowCommand {
        window,
        kind: WindowCommandKind::EndInteraction,
    });
    commands.entity(window).insert(state);
    if *target.visibility == WindowVisibility::Visible {
        commands.trigger(WindowCommand {
            window,
            kind: WindowCommandKind::Focus,
        });
    }
    redraw.write(RequestRedraw);
}

fn protocol_requests(
    requests: Query<(Entity, &PendingClientFullscreen, &OccupiesWindow)>,
    windows: Query<(&WindowOutput, &WindowVisibility)>,
    focus: Res<FocusedWindow>,
    mut commands: Commands,
) {
    for (client, request, occupant) in &requests {
        let window = occupant.0;
        let Ok((_, visibility)) = windows.get(window) else {
            continue;
        };
        commands.entity(client).remove::<PendingClientFullscreen>();
        if request.0 && focus.entity() != Some(window) && *visibility == WindowVisibility::Visible {
            continue;
        }
        commands.trigger(FullscreenRequest {
            window: Some(window),
            action: if request.0 {
                FullscreenAction::Enable(FullscreenMode::Normal)
            } else {
                FullscreenAction::Disable
            },
        });
    }
}

#[derive(QueryData)]
#[query_data(mutable)]
struct PresentedWindow {
    entity: Entity,
    output: &'static WindowOutput,
    geometry: &'static mut WindowGeometry,
    z: &'static mut WindowZOrder,
    visibility: &'static WindowVisibility,
    occupant: Option<&'static WindowOccupant>,
    vacancy: &'static WindowVacancy,
    fullscreen: Option<&'static WindowFullscreen>,
    occluded: Option<&'static FullscreenOccluded>,
}

fn belongs_to_application(
    window: Entity,
    owner: Entity,
    clients: &WindowClientResolver,
    parents: &Query<&ClientToplevelParent>,
) -> bool {
    let mut current = window;
    for _ in 0..64 {
        if current == owner {
            return true;
        }
        let Some(client) = clients.client_entity(current) else {
            return false;
        };
        let Ok(parent) = parents.get(client) else {
            return false;
        };
        let Some(parent) = clients.window_for_surface(parent.surface) else {
            return false;
        };
        current = parent;
    }
    false
}

type FullscreenOutputs<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static OutputGeometry,
        Option<&'static DesktopLayerVisibility>,
        Option<&'static FullscreenOutput>,
    ),
    With<WeldOutput>,
>;

fn reconcile(
    mut windows: Query<PresentedWindow, With<ManagedWindow>>,
    outputs: FullscreenOutputs,
    clients: WindowClientResolver,
    parents: Query<&ClientToplevelParent>,
    focus: Res<FocusedWindow>,
    mut commands: Commands,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let mut active = Vec::new();
    for mut window in &mut windows {
        let Some(state) = window.fullscreen else {
            continue;
        };
        if *window.visibility != WindowVisibility::Visible {
            continue;
        }
        let presentable = if window.occupant.is_some() {
            clients.mapped_client(window.entity).is_some()
        } else {
            *window.vacancy == WindowVacancy::Retain
        };
        if !presentable {
            continue;
        }
        let Ok((_, output, _, _)) = outputs.get(window.output.0) else {
            continue;
        };
        let geometry = WindowGeometry {
            position: Vec2::ZERO,
            size: output.logical_size(),
        };
        if *window.geometry != geometry {
            *window.geometry = geometry;
            redraw.write(RequestRedraw);
        }
        // Related floating dialogs remain above the output-sized owner.
        if window.z.0 != 0 {
            window.z.0 = 0;
        }
        active.push((window.output.0, window.entity, state.mode));
    }
    // Workspace/output transfers can bring two retained claims together.
    // Prefer the selected application, then use stable entity order.
    active.sort_unstable_by_key(|(output, window, _)| {
        (
            output.to_bits(),
            !focus.entity().is_some_and(|selected| {
                belongs_to_application(selected, *window, &clients, &parents)
            }),
            window.to_bits(),
        )
    });
    let mut claimed_outputs = HashSet::new();
    active.retain(|(output, window, _)| {
        if claimed_outputs.insert(*output) {
            true
        } else {
            commands.trigger(FullscreenRequest {
                window: Some(*window),
                action: FullscreenAction::Disable,
            });
            false
        }
    });
    for window in &mut windows {
        let owner = active
            .iter()
            .find(|(output, _, _)| *output == window.output.0);
        let hidden = owner.is_some_and(|(_, owner, _)| {
            !belongs_to_application(window.entity, *owner, &clients, &parents)
        });
        if hidden != window.occluded.is_some() {
            if hidden {
                commands.entity(window.entity).insert(FullscreenOccluded);
            } else {
                commands
                    .entity(window.entity)
                    .remove::<FullscreenOccluded>();
            }
            redraw.write(RequestRedraw);
        }
    }
    for (output, _, layers, owned) in &outputs {
        let claim = active.iter().find(|(id, _, _)| *id == output);
        let policy = claim.map(|(_, _, mode)| match mode {
            FullscreenMode::Normal => DesktopLayerVisibility::OverlayOnly,
            FullscreenMode::Exclusive => DesktopLayerVisibility::Hidden,
        });
        if let (Some(policy), Some((_, window, mode))) = (policy, claim) {
            if layers != Some(&policy)
                || owned.is_none_or(|owned| owned.window != *window || owned.mode != *mode)
            {
                let previous_layers = owned.map_or(layers.copied(), |owned| owned.previous_layers);
                commands.entity(output).insert((
                    policy,
                    FullscreenOutput {
                        window: *window,
                        mode: *mode,
                        previous_layers,
                    },
                ));
                redraw.write(RequestRedraw);
            }
        } else if let Some(owned) = owned {
            let mut output = commands.entity(output);
            output.remove::<FullscreenOutput>();
            if let Some(previous) = owned.previous_layers {
                output.insert(previous);
            } else {
                output.remove::<DesktopLayerVisibility>();
            }
            redraw.write(RequestRedraw);
        }
    }
    if let Some(selected) = focus.entity()
        && let Ok(window) = windows.get(selected)
        && let Some((_, owner, _)) = active
            .iter()
            .find(|(output, _, _)| *output == window.output.0)
        && !belongs_to_application(selected, *owner, &clients, &parents)
    {
        commands.trigger(WindowCommand {
            window: *owner,
            kind: WindowCommandKind::Focus,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WindowId, WindowPlugin, WindowVacancy};
    use bevy::math::UVec2;
    use weld_app::{output::OutputId, surface::SurfaceActionQueue};

    #[test]
    fn colliding_visible_workspace_claims_keep_the_selected_owner_and_restore_layer_policy() {
        let mut app = App::new();
        app.init_resource::<SurfaceActionQueue>()
            .add_plugins((WindowPlugin, FullscreenPlugin));
        let output = app
            .world_mut()
            .spawn((
                WeldOutput {
                    id: OutputId::new(1),
                },
                OutputGeometry::from_physical(UVec2::new(800, 600), 1.0),
                DesktopLayerVisibility::OverlayOnly,
            ))
            .id();
        let mut windows = Vec::new();
        for id in 1..=2 {
            let workspace = app.world_mut().spawn_empty().id();
            let window = app
                .world_mut()
                .spawn((
                    ManagedWindow {
                        id: WindowId::new(id),
                    },
                    WindowVacancy::Retain,
                    WindowOutput(output),
                    WindowVisibility::Visible,
                    WorkspaceMember(workspace),
                ))
                .id();
            windows.push(window);
            app.world_mut().trigger(FullscreenRequest {
                window: Some(window),
                action: FullscreenAction::Enable(FullscreenMode::Exclusive),
            });
            app.update();
        }
        app.update();
        assert!(app.world().get::<WindowFullscreen>(windows[0]).is_none());
        assert!(app.world().get::<WindowFullscreen>(windows[1]).is_some());
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(windows[1])
        );
        app.world_mut().trigger(FullscreenRequest {
            window: None,
            action: FullscreenAction::Toggle(FullscreenMode::Normal),
        });
        app.update();
        assert!(app.world().get::<WindowFullscreen>(windows[1]).is_none());
        assert!(app.world().get::<FullscreenOutput>(output).is_none());
        assert_eq!(
            app.world().get::<DesktopLayerVisibility>(output),
            Some(&DesktopLayerVisibility::OverlayOnly)
        );
    }
}
