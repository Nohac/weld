//! Shared mouse and native-grab input for window-management policies.
//!
//! Managers accept [`PointerInteractionRequest`] after checking ownership and
//! geometry, then consume [`WindowIntentKind::MoveBy`] or
//! [`WindowIntentKind::ResizeBy`]. This module owns chord capture, motion batching
//! and matching release; managers own geometry and their settlement state.

use crate::{
    FocusedWindow, ManagedBy, WindowClientResolver, WindowCloseHandle, WindowCommand,
    WindowCommandKind, WindowGeometry, WindowIntent, WindowIntentKind, WindowInteractionKind,
    WindowInteractionSession, WindowMoveHandle, WindowOutput, WindowProjectionLookup,
    WindowResizeHandle, WindowSystems, WindowVisibility,
    fullscreen::{FullscreenOccluded, WindowFullscreen},
};
use bevy::{
    app::{App, Plugin, PreUpdate},
    ecs::{
        bundle::Bundle,
        change_detection::{DetectChanges, Ref},
        component::Component,
        entity::Entity,
        event::Event,
        message::{MessageReader, MessageWriter},
        observer::On,
        query::{With, Without},
        resource::Resource,
        schedule::{ApplyDeferred, IntoScheduleConfigs, SystemSet},
        system::{Commands, Local, Query, Res, ResMut, SystemParam},
    },
    input::{
        ButtonInput, ButtonState,
        mouse::{MouseButton, MouseButtonInput, MouseMotion},
    },
    math::Vec2,
    picking::{
        PickingSystems,
        events::{Click, Pointer, Press},
        pointer::PointerButton,
    },
    window::RequestRedraw,
};
use std::collections::{HashMap, HashSet};
use weld_app::{
    input::{PointerShortcut, PointerShortcutId, PointerShortcutModifiers, PointerShortcutPressed},
    output::{OutputPosition, WeldOutput},
    surface::{
        ClientDecorated, ClientToplevel, MappedSurface, ToplevelInteractionRequest,
        ToplevelInteractionRequestKind, ToplevelResizeEdge,
    },
};
use weld_input::{
    PointerShortcutRegistry, PointerShortcutSet, PublishedPointerTarget, register_pointer_shortcuts,
};

/// Live compositor pointer chords and focus policy, configured by the distribution.
#[derive(Resource, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowPointerSettings {
    pub modifier: Option<PointerShortcutModifiers>,
    /// Focus a window when mouse motion enters it, preserving its stacking order.
    pub focus_follows_mouse: bool,
}

/// A picked handle/chord or host-validated native grab offered to the manager.
/// The manager checks window ownership and support for the requested operation.
#[derive(Event, Clone, Copy, Debug)]
pub struct PointerInteractionRequest {
    pub window: Entity,
    pub kind: WindowInteractionKind,
    pub control: PointerInteractionControl,
}

impl PointerInteractionRequest {
    /// Enqueue a session and its policy-owned state after validating geometry.
    ///
    /// Existing sessions on this window, fullscreen windows and retired windows
    /// are ignored. Acceptance ends a session on another window, and publishes
    /// the input owner and `state` together. The manager retires its state on
    /// [`WindowIntentKind::InteractionEnded`]; this module retires the controller.
    pub fn accept(self, commands: &mut Commands, state: impl Bundle) {
        // Apply the lifetime, input owner and policy anchor as one discrete
        // transaction, including when several requests share an input batch.
        commands.queue(move |world: &mut bevy::ecs::world::World| {
            if world.get::<WindowInteractionSession>(self.window).is_some()
                || world.get::<WindowFullscreen>(self.window).is_some()
            {
                return;
            }
            super::begin_window_interaction(world, self.window, self.kind);
            if world.get::<WindowInteractionSession>(self.window).is_some()
                && let Ok(mut window) = world.get_entity_mut(self.window)
            {
                window.insert((self.control, state));
            }
        });
    }
}

#[derive(Component, Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerInteractionControl {
    Pointer(MouseButton),
    Protocol,
}

#[derive(SystemSet, Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum WindowPointerSystems {
    /// Chords resolve after admission and before layout/settlement policy runs.
    Start,
}

/// Translates native grabs and pointer affordances into manager-owned sessions.
pub struct WindowPointerPlugin;

impl Plugin for WindowPointerPlugin {
    fn build(&self, app: &mut App) {
        register_pointer_shortcuts(app);
        app.init_resource::<WindowPointerSettings>()
            .init_resource::<WindowPointerShortcuts>()
            .init_resource::<ShortcutCapture>()
            .add_message::<MouseButtonInput>()
            .add_message::<MouseMotion>()
            .add_message::<ToplevelInteractionRequest>()
            .configure_sets(
                PreUpdate,
                WindowPointerSystems::Start.in_set(WindowSystems::Management),
            )
            .add_observer(activate_window)
            .add_observer(begin_move_handle)
            .add_observer(begin_resize_handle)
            .add_observer(close_window)
            .add_observer(clear_control)
            .add_systems(
                PreUpdate,
                focus_under_pointer
                    .after(PickingSystems::Last)
                    .after(WindowSystems::InteractionFinalize)
                    .before(WindowSystems::FinalReconcile),
            )
            .add_systems(
                PreUpdate,
                (synchronize_shortcuts, begin_pointer_shortcut_interactions)
                    .chain()
                    .in_set(WindowPointerSystems::Start),
            )
            .add_systems(
                PreUpdate,
                handle_protocol_interactions.in_set(WindowSystems::Interaction),
            )
            .add_systems(
                PreUpdate,
                (
                    drive_pointer_interactions,
                    ApplyDeferred,
                    end_pointer_interactions,
                    ApplyDeferred,
                )
                    .chain()
                    .in_set(WindowSystems::InteractionFinalize),
            );
    }
}

#[derive(SystemParam)]
struct HoverFocus<'w, 's> {
    motions: MessageReader<'w, 's, MouseMotion>,
    buttons: Option<Res<'w, ButtonInput<MouseButton>>>,
    settings: Res<'w, WindowPointerSettings>,
    target: Res<'w, PublishedPointerTarget>,
    projections: WindowProjectionLookup<'w, 's>,
    windows:
        Query<'w, 's, &'static WindowVisibility, (With<ManagedBy>, Without<FullscreenOccluded>)>,
    sessions: Query<'w, 's, (), With<WindowInteractionSession>>,
    focus: Res<'w, FocusedWindow>,
}

fn focus_under_pointer(
    mut hover: HoverFocus,
    mut previous: Local<Option<Entity>>,
    mut commands: Commands,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    // Use the frontmost published hit, including decoration and popup ancestry.
    // Refresh the remembered window even without motion: layout changes and
    // keyboard navigation must not turn a stationary pointer into a focus request.
    let target = hover
        .target
        .0
        .and_then(|target| hover.projections.window_for(target))
        .filter(|window| {
            hover
                .windows
                .get(*window)
                .is_ok_and(|visibility| *visibility == WindowVisibility::Visible)
        });
    let entered = *previous != target;
    *previous = target;
    let mut moved = false;
    for motion in hover.motions.read() {
        moved |= motion.delta.is_finite() && motion.delta != Vec2::ZERO;
    }
    let held = hover.buttons.as_ref().is_some_and(|buttons| {
        buttons.get_pressed().next().is_some() || buttons.get_just_released().next().is_some()
    });
    if !hover.settings.focus_follows_mouse
        || !entered
        || !moved
        || held
        || !hover.sessions.is_empty()
    {
        return;
    }
    if let Some(window) = target.filter(|window| hover.focus.entity() != Some(*window)) {
        commands.trigger(WindowCommand {
            window,
            kind: WindowCommandKind::Focus,
        });
        redraw.write(RequestRedraw);
    }
}

fn clear_control(event: On<WindowIntent>, mut commands: Commands) {
    if matches!(event.kind, WindowIntentKind::InteractionEnded(_)) {
        commands
            .entity(event.window)
            .try_remove::<PointerInteractionControl>();
    }
}

#[derive(Resource, Default)]
struct WindowPointerShortcuts {
    move_window: Option<PointerShortcutId>,
    resize_window: Option<PointerShortcutId>,
    owned: PointerShortcutSet,
}

fn synchronize_shortcuts(
    settings: Res<WindowPointerSettings>,
    mut shortcuts: ResMut<WindowPointerShortcuts>,
    mut registry: ResMut<PointerShortcutRegistry>,
) {
    if !settings.is_changed() {
        return;
    }
    let bindings = settings.modifier.into_iter().flat_map(|modifier| {
        [
            PointerShortcut::new(MouseButton::Left, modifier),
            PointerShortcut::new(MouseButton::Right, modifier),
        ]
    });
    let ids = shortcuts.owned.replace(&mut registry, bindings);
    shortcuts.move_window = ids.first().copied();
    shortcuts.resize_window = ids.get(1).copied();
}

#[derive(Resource, Default)]
struct ShortcutCapture(HashSet<MouseButton>);

#[derive(SystemParam)]
struct PointerTargets<'w, 's> {
    projections: WindowProjectionLookup<'w, 's>,
    fullscreen: Query<'w, 's, (), With<WindowFullscreen>>,
    windows: Query<'w, 's, Option<&'static WindowInteractionSession>, With<ManagedBy>>,
}

impl PointerTargets<'_, '_> {
    fn available_window_for(&self, target: Entity) -> Option<Entity> {
        let window = self.projections.window_for(target)?;
        self.windows
            .get(window)
            .ok()
            .filter(|interaction| interaction.is_none() && !self.fullscreen.contains(window))
            .map(|_| window)
    }

    fn owned_window_for(&self, target: Entity) -> Option<Entity> {
        let window = self.projections.window_for(target)?;
        self.windows.get(window).ok().map(|_| window)
    }
}

fn activate_window(
    mut press: On<Pointer<Press>>,
    capture: Res<ShortcutCapture>,
    targets: PointerTargets,
    mut commands: Commands,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if press.button != PointerButton::Primary || capture.0.contains(&MouseButton::Left) {
        return;
    }
    let Some(window) = targets.owned_window_for(press.entity) else {
        return;
    };
    press.propagate(false);
    commands.trigger(WindowIntent {
        window,
        kind: WindowIntentKind::Activate,
    });
    redraw.write(RequestRedraw);
}

fn begin_move_handle(
    press: On<Pointer<Press>>,
    capture: Res<ShortcutCapture>,
    handles: Query<(), With<WindowMoveHandle>>,
    targets: PointerTargets,
    mut commands: Commands,
) {
    if press.button != PointerButton::Primary
        || capture.0.contains(&MouseButton::Left)
        || !handles.contains(press.entity)
        || press.original_event_target() != press.entity
    {
        return;
    }
    let Some(window) = targets.available_window_for(press.entity) else {
        return;
    };
    request_interaction(
        &mut commands,
        window,
        WindowInteractionKind::Move,
        PointerInteractionControl::Pointer(MouseButton::Left),
    );
}

fn begin_resize_handle(
    press: On<Pointer<Press>>,
    capture: Res<ShortcutCapture>,
    handles: Query<&WindowResizeHandle>,
    targets: PointerTargets,
    mut commands: Commands,
) {
    if press.button != PointerButton::Primary
        || capture.0.contains(&MouseButton::Left)
        || press.original_event_target() != press.entity
    {
        return;
    }
    let Ok(handle) = handles.get(press.entity) else {
        return;
    };
    let Some(window) = targets.available_window_for(press.entity) else {
        return;
    };
    request_interaction(
        &mut commands,
        window,
        WindowInteractionKind::Resize(handle.0),
        PointerInteractionControl::Pointer(MouseButton::Left),
    );
}

fn close_window(
    mut click: On<Pointer<Click>>,
    capture: Res<ShortcutCapture>,
    handles: Query<(), With<WindowCloseHandle>>,
    targets: PointerTargets,
    mut commands: Commands,
) {
    if click.button != PointerButton::Primary
        || capture.0.contains(&MouseButton::Left)
        || !handles.contains(click.entity)
        || click.original_event_target() != click.entity
    {
        return;
    }
    let Some(window) = targets.owned_window_for(click.entity) else {
        return;
    };
    click.propagate(false);
    commands.trigger(WindowIntent {
        window,
        kind: WindowIntentKind::CloseRequested,
    });
}

fn handle_protocol_interactions(
    mut requests: MessageReader<ToplevelInteractionRequest>,
    surfaces: Query<(
        &ClientToplevel,
        Option<&MappedSurface>,
        Option<&ClientDecorated>,
    )>,
    clients: WindowClientResolver,
    windows: ProtocolInteractionWindows,
    mut commands: Commands,
) {
    for request in requests.read().copied() {
        let Some((_, Some(_), Some(_))) = surfaces
            .iter()
            .find(|(toplevel, _, _)| toplevel.surface == request.surface)
        else {
            continue;
        };
        let Some(window) = clients.window_for_surface(request.surface) else {
            continue;
        };
        let Ok((interaction, control)) = windows.get(window) else {
            continue;
        };
        match request.kind {
            ToplevelInteractionRequestKind::Move if interaction.is_none() => {
                request_interaction(
                    &mut commands,
                    window,
                    WindowInteractionKind::Move,
                    PointerInteractionControl::Protocol,
                );
            }
            ToplevelInteractionRequestKind::Resize { edges } if interaction.is_none() => {
                request_interaction(
                    &mut commands,
                    window,
                    WindowInteractionKind::Resize(edges),
                    PointerInteractionControl::Protocol,
                );
            }
            ToplevelInteractionRequestKind::End
                if interaction.is_some()
                    && control == Some(&PointerInteractionControl::Protocol) =>
            {
                commands.trigger(WindowCommand {
                    window,
                    kind: WindowCommandKind::EndInteraction,
                });
            }
            ToplevelInteractionRequestKind::Move
            | ToplevelInteractionRequestKind::Resize { .. }
            | ToplevelInteractionRequestKind::End => {}
        }
    }
}

type InteractiveWindows = (With<ManagedBy>, Without<WindowFullscreen>);

type ProtocolInteractionWindows<'w, 's> = Query<
    'w,
    's,
    (
        Option<&'static WindowInteractionSession>,
        Option<&'static PointerInteractionControl>,
    ),
    InteractiveWindows,
>;

#[derive(SystemParam)]
struct PointerShortcutInteractionParams<'w, 's> {
    presses: MessageReader<'w, 's, PointerShortcutPressed>,
    button_inputs: MessageReader<'w, 's, MouseButtonInput>,
    shortcuts: Res<'w, WindowPointerShortcuts>,
    capture: ResMut<'w, ShortcutCapture>,
    projections: WindowProjectionLookup<'w, 's>,
    windows: Query<
        'w,
        's,
        (
            &'static WindowGeometry,
            &'static WindowOutput,
            Option<&'static WindowInteractionSession>,
        ),
        InteractiveWindows,
    >,
    output_positions: Query<'w, 's, &'static OutputPosition, With<WeldOutput>>,
    commands: Commands<'w, 's>,
}

fn begin_pointer_shortcut_interactions(params: PointerShortcutInteractionParams) {
    let PointerShortcutInteractionParams {
        mut presses,
        mut button_inputs,
        shortcuts,
        mut capture,
        projections,
        windows,
        output_positions,
        mut commands,
    } = params;
    let final_button_states = button_inputs
        .read()
        .map(|input| (input.button, input.state))
        .collect::<HashMap<_, _>>();
    for press in presses.read().copied() {
        let (button, shortcut_kind) = if Some(press.shortcut()) == shortcuts.move_window {
            (MouseButton::Left, None)
        } else if Some(press.shortcut()) == shortcuts.resize_window {
            (MouseButton::Right, Some(press.position()))
        } else {
            continue;
        };
        // A registered shell chord is consumed even when it lands on the
        // background or on a window managed by another policy.
        capture.0.insert(button);
        if final_button_states.get(&button) == Some(&ButtonState::Released) {
            continue;
        }
        let Some(window) = press
            .target()
            .and_then(|target| projections.window_for(target))
        else {
            continue;
        };
        let Ok((geometry, output, interaction)) = windows.get(window) else {
            continue;
        };
        if interaction.is_some() {
            continue;
        }
        let kind = if let Some(position) = shortcut_kind {
            let Some(position) = position else {
                continue;
            };
            let Ok(output_position) = output_positions.get(output.0) else {
                continue;
            };
            let Some(edge) = resize_edge_from_position(position, *geometry, output_position.0)
            else {
                continue;
            };
            WindowInteractionKind::Resize(edge)
        } else {
            WindowInteractionKind::Move
        };
        commands.trigger(WindowIntent {
            window,
            kind: WindowIntentKind::Activate,
        });
        request_interaction(
            &mut commands,
            window,
            kind,
            PointerInteractionControl::Pointer(button),
        );
    }
}

fn request_interaction(
    commands: &mut Commands,
    window: Entity,
    kind: WindowInteractionKind,
    control: PointerInteractionControl,
) {
    commands.trigger(PointerInteractionRequest {
        window,
        kind,
        control,
    });
}

fn resize_edge_from_position(
    position: Vec2,
    geometry: WindowGeometry,
    output_position: Vec2,
) -> Option<ToplevelResizeEdge> {
    if !position.is_finite()
        || !geometry.position.is_finite()
        || !geometry.size.is_finite()
        || geometry.size.cmple(Vec2::ZERO).any()
        || !output_position.is_finite()
    {
        return None;
    }
    let center = output_position + geometry.position + geometry.size * 0.5;
    Some(match (position.x > center.x, position.y > center.y) {
        (false, false) => ToplevelResizeEdge::TopLeft,
        (true, false) => ToplevelResizeEdge::TopRight,
        (false, true) => ToplevelResizeEdge::BottomLeft,
        (true, true) => ToplevelResizeEdge::BottomRight,
    })
}

/// Projects frame-paced mouse motion into managed-window intents.
///
/// Pointer controls ignore the update that created them, preventing motion
/// that preceded the press from entering the new session. Protocol controls
/// have already passed Smithay's live-grab validation and may consume motion
/// from their creation update. Other input plugins can drive the same public
/// move and resize intents without going through this mouse adapter.
fn drive_pointer_interactions(
    mut motions: MessageReader<MouseMotion>,
    mut button_inputs: MessageReader<MouseButtonInput>,
    mut held_buttons: Local<HashSet<MouseButton>>,
    sessions: Query<(
        Entity,
        &WindowInteractionSession,
        &PointerInteractionControl,
    )>,
    mut commands: Commands,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let held_before = held_buttons.clone();
    for input in button_inputs.read() {
        match input.state {
            ButtonState::Pressed => {
                held_buttons.insert(input.button);
            }
            ButtonState::Released => {
                held_buttons.remove(&input.button);
            }
        }
    }
    let delta = motions
        .read()
        .filter_map(|motion| motion.delta.is_finite().then_some(motion.delta))
        .sum::<Vec2>();
    if delta == Vec2::ZERO {
        return;
    }

    let mut moved = false;
    for (window, session, control) in &sessions {
        let accepts_motion = match control {
            PointerInteractionControl::Pointer(button) => held_before.contains(button),
            PointerInteractionControl::Protocol => true,
        };
        if !accepts_motion {
            continue;
        }
        let kind = match session.kind {
            WindowInteractionKind::Move => WindowIntentKind::MoveBy(delta),
            WindowInteractionKind::Resize(_) => WindowIntentKind::ResizeBy(delta),
        };
        commands.trigger(WindowIntent { window, kind });
        moved = true;
    }
    if moved {
        redraw.write(RequestRedraw);
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ButtonBatchState {
    saw_release: bool,
    final_state: Option<ButtonState>,
}

fn end_pointer_interactions(
    mut button_inputs: MessageReader<MouseButtonInput>,
    mut capture: ResMut<ShortcutCapture>,
    sessions: Query<(
        Entity,
        Ref<WindowInteractionSession>,
        &PointerInteractionControl,
    )>,
    mut commands: Commands,
) {
    let mut states = HashMap::<MouseButton, ButtonBatchState>::new();
    for input in button_inputs.read() {
        let state = states.entry(input.button).or_default();
        state.saw_release |= input.state == ButtonState::Released;
        state.final_state = Some(input.state);
        if input.state == ButtonState::Released {
            capture.0.remove(&input.button);
        }
    }
    for (window, session, control) in &sessions {
        let PointerInteractionControl::Pointer(button) = control else {
            continue;
        };
        let Some(state) = states.get(button) else {
            continue;
        };
        let should_end = match state.final_state {
            Some(ButtonState::Released) => true,
            Some(ButtonState::Pressed) if state.saw_release => !session.is_added(),
            Some(ButtonState::Pressed) | None => false,
        };
        if should_end {
            commands.entity(window).trigger(|window| WindowCommand {
                window,
                kind: WindowCommandKind::EndInteraction,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ManagedWindow, WindowId, WindowPlugin, WindowProjection, WindowVacancy, WindowZOrder,
    };
    use bevy::ecs::{hierarchy::ChildOf, message::Messages};
    use weld_app::surface::SurfaceActionQueue;

    fn hover_app() -> (App, Entity, Entity, Entity, Entity) {
        let mut app = App::new();
        app.init_resource::<SurfaceActionQueue>()
            .init_resource::<ButtonInput<MouseButton>>()
            .add_plugins((WindowPlugin, WindowPointerPlugin));
        app.world_mut()
            .resource_mut::<WindowPointerSettings>()
            .focus_follows_mouse = true;
        let owner = app.world_mut().spawn_empty().id();
        let first = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(1),
                },
                ManagedBy(owner),
                WindowVacancy::Retain,
                WindowZOrder(2),
            ))
            .id();
        let second = app
            .world_mut()
            .spawn((
                ManagedWindow {
                    id: WindowId::new(2),
                },
                ManagedBy(owner),
                WindowVacancy::Retain,
                WindowZOrder(1),
            ))
            .id();
        let first_root = app
            .world_mut()
            .spawn(WindowProjection::new(first, owner))
            .id();
        let second_root = app
            .world_mut()
            .spawn(WindowProjection::new(second, owner))
            .id();
        app.world_mut().trigger(WindowCommand {
            window: first,
            kind: WindowCommandKind::Focus,
        });
        (app, first, second, first_root, second_root)
    }

    fn hover(app: &mut App, target: Option<Entity>, moved: bool) {
        app.world_mut().resource_mut::<PublishedPointerTarget>().0 = target;
        if moved {
            app.world_mut()
                .resource_mut::<Messages<MouseMotion>>()
                .write(MouseMotion { delta: Vec2::X });
        }
        app.update();
    }

    #[test]
    fn hover_focus_resolves_descendants_without_raising_or_undoing_keyboard_focus() {
        let (mut app, first, second, first_root, second_root) = hover_app();
        let child = app.world_mut().spawn(ChildOf(second_root)).id();
        hover(&mut app, Some(first_root), true);
        hover(&mut app, Some(child), true);
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(second)
        );
        assert_eq!(app.world().get::<WindowZOrder>(second).expect("stack").0, 1);
        app.world_mut().trigger(WindowCommand {
            window: first,
            kind: WindowCommandKind::Focus,
        });
        hover(&mut app, Some(second_root), true);
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(first)
        );
        hover(&mut app, None, true);
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(first)
        );
        hover(&mut app, Some(child), true);
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(second)
        );
    }

    #[test]
    fn stationary_relayout_and_live_disable_do_not_steal_focus() {
        let (mut app, first, second, first_root, second_root) = hover_app();
        hover(&mut app, Some(first_root), true);
        hover(&mut app, Some(second_root), false);
        hover(&mut app, Some(second_root), true);
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(first)
        );
        app.world_mut()
            .resource_mut::<WindowPointerSettings>()
            .focus_follows_mouse = false;
        hover(&mut app, None, true);
        hover(&mut app, Some(second_root), true);
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(first)
        );
        app.world_mut()
            .resource_mut::<WindowPointerSettings>()
            .focus_follows_mouse = true;
        hover(&mut app, Some(first_root), true);
        hover(&mut app, Some(second_root), true);
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(second)
        );
    }

    #[test]
    fn hover_focus_respects_buttons_sessions_visibility_and_fullscreen_occlusion() {
        let (mut app, first, second, _, second_root) = hover_app();
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);
        hover(&mut app, Some(second_root), true);
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(first)
        );
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .reset_all();
        app.world_mut()
            .entity_mut(first)
            .insert(WindowInteractionSession {
                kind: WindowInteractionKind::Move,
            });
        hover(&mut app, None, true);
        hover(&mut app, Some(second_root), true);
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(first)
        );
        app.world_mut()
            .entity_mut(first)
            .remove::<WindowInteractionSession>();
        app.world_mut()
            .entity_mut(second)
            .insert(WindowVisibility::Hidden);
        hover(&mut app, Some(second_root), true);
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(first)
        );
        app.world_mut()
            .entity_mut(second)
            .insert((WindowVisibility::Visible, FullscreenOccluded));
        hover(&mut app, Some(second_root), true);
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(first)
        );
        app.world_mut()
            .entity_mut(second)
            .remove::<FullscreenOccluded>();
        hover(&mut app, Some(second_root), true);
        assert_eq!(
            app.world().resource::<FocusedWindow>().entity(),
            Some(second)
        );
    }

    #[test]
    fn modifier_resize_selects_the_pointer_quadrant() {
        let geometry = WindowGeometry {
            position: Vec2::new(100.0, 50.0),
            size: Vec2::new(200.0, 100.0),
        };
        let output = Vec2::new(1_000.0, 500.0);

        assert_eq!(
            resize_edge_from_position(Vec2::new(1_150.0, 575.0), geometry, output),
            Some(ToplevelResizeEdge::TopLeft)
        );
        assert_eq!(
            resize_edge_from_position(Vec2::new(1_250.0, 575.0), geometry, output),
            Some(ToplevelResizeEdge::TopRight)
        );
        assert_eq!(
            resize_edge_from_position(Vec2::new(1_150.0, 625.0), geometry, output),
            Some(ToplevelResizeEdge::BottomLeft)
        );
        assert_eq!(
            resize_edge_from_position(Vec2::new(1_250.0, 625.0), geometry, output),
            Some(ToplevelResizeEdge::BottomRight)
        );
        assert_eq!(
            resize_edge_from_position(Vec2::new(1_200.0, 600.0), geometry, output),
            Some(ToplevelResizeEdge::TopLeft)
        );
    }
}
