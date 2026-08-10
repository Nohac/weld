//! Bevy/Leafwing projection and client-surface routing for raw seat input.
//!
//! [`RawSeatEvent`] values are retained verbatim for protocol delivery while a
//! same-update projection feeds standard Bevy input and Leafwing's
//! [`CentralInputStorePlugin`]. Bevy picking resolves the topmost exact
//! [`SurfaceInputNode`], then this module emits typed [`SeatInputEffect`]
//! values keyed by [`SurfaceId`] and surface layer. Smithay resources never
//! enter the ECS world.
//!
//! Keyboard focus is click-to-focus in the initial slice. A future shortcut
//! policy belongs after [`InputManagerSystem::Update`] and before protocol
//! routing; paired interception must annotate rather than replace the lossless
//! raw stream. That early action phase cannot consult the same update's final
//! picked surface without a later policy phase. Invisible
//! [`SurfaceInputNode`] rectangles are the picking boundary; visual surface
//! images do not accept input directly.

use std::collections::{HashSet, VecDeque};

use bevy::{
    app::{App, First, Plugin, PreUpdate},
    camera::NormalizedRenderTarget,
    ecs::{
        entity::Entity,
        message::{MessageUpdateSystems, MessageWriter},
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Query, Res, ResMut, SystemParam},
        world::World,
    },
    input::{
        ButtonState,
        keyboard::{Key, KeyCode, KeyboardFocusLost, KeyboardInput, NativeKey},
        mouse::{MouseButton, MouseButtonInput, MouseMotion, MouseScrollUnit, MouseWheel},
        touch::TouchPhase,
    },
    math::Vec2,
    picking::{
        PickingSystems,
        pointer::{
            Location, PointerAction, PointerId, PointerInput, PointerInteraction, PointerLocation,
        },
    },
    ui::{ComputedNode, Display, Node, UiScale},
};
use bevy_winit::converters::convert_physical_key_code;
use leafwing_input_manager::plugin::{CentralInputStorePlugin, InputManagerSystem};
use tracing::{trace, warn};
use winit::{keyboard::PhysicalKey, platform::scancode::PhysicalKeyExtScancode};

use crate::{
    raw_input::{
        InputPosition, LinuxButtonCode, LinuxKeycode, RawScrollFrame, RawScrollPhase, RawSeatEvent,
        RawSeatEventKind,
    },
    surface::{SurfaceId, SurfaceInputNode, SurfaceLayerId},
};

// Weld has no Bevy Window entity: the manual render target is not a window.
// Bevy input state systems and Leafwing intentionally ignore this field.
const INPUT_WINDOW: Entity = Entity::PLACEHOLDER;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SurfaceHit {
    pub surface: SurfaceId,
    pub layer: SurfaceLayerId,
    pub local_position: InputPosition,
}

/// Owned policy result consumed and validated by the Smithay host.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SeatInputEffect {
    pub event: SeatInputEffectKind,
    pub time: u32,
}

impl SeatInputEffect {
    const fn new(event: SeatInputEffectKind, time: u32) -> Self {
        Self { event, time }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum SeatInputEffectKind {
    PointerMotion {
        position: InputPosition,
        target: Option<SurfaceHit>,
    },
    PointerButton {
        position: InputPosition,
        target: Option<SurfaceHit>,
        button: LinuxButtonCode,
        state: ButtonState,
    },
    PointerAxis {
        axis: RawScrollFrame,
    },
    Keyboard {
        keycode: LinuxKeycode,
        state: ButtonState,
    },
    HostFocusLost,
}

pub(crate) struct InputBridgePlugin {
    target: NormalizedRenderTarget,
}

impl InputBridgePlugin {
    /// Build the input bridge for Weld's manual composition target.
    ///
    /// Input-only host advances no longer imply a rendered composition.
    /// Picking observers and action systems that mutate visuals must emit
    /// [`bevy::window::RequestRedraw`].
    pub(crate) const fn new(target: NormalizedRenderTarget) -> Self {
        Self { target }
    }
}

impl Plugin for InputBridgePlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<CentralInputStorePlugin>() {
            app.add_plugins(CentralInputStorePlugin);
        }
        app.insert_resource(InputTarget(self.target.clone()))
            .init_resource::<UiScale>()
            .init_resource::<RawInputIngress>()
            .init_resource::<PendingSeatInput>()
            .init_resource::<ProjectedPointerState>()
            .init_resource::<ProjectedMouseButtons>()
            .init_resource::<InputEffects>()
            .init_resource::<InputUpdateTime>()
            .init_resource::<PointerRoutingState>()
            .add_systems(First, project_raw_input.after(MessageUpdateSystems))
            .add_systems(
                PreUpdate,
                resolve_input_effects
                    .in_set(PickingSystems::PostHover)
                    .after(InputManagerSystem::Update),
            );
    }
}

#[derive(Resource)]
struct InputTarget(NormalizedRenderTarget);

#[derive(Resource, Default)]
struct RawInputIngress(VecDeque<RawSeatEvent>);

#[derive(Resource, Default)]
struct PendingSeatInput(VecDeque<RawSeatEvent>);

/// Projection and protocol routing deliberately own separate instances: the
/// former advances in `First`, while the latter replays lossless events in
/// `PreUpdate`. Merging them collapses every batch to its final pointer state.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PointerPositionState {
    host_position: Option<InputPosition>,
    last_known_position: InputPosition,
}

impl PointerPositionState {
    fn apply(&mut self, position: InputPosition) {
        self.host_position = Some(position);
        self.last_known_position = position;
    }

    /// Clear host presence without discarding the location used for cancel or
    /// position-less button and axis events.
    fn clear_host(&mut self) {
        self.host_position = None;
    }
}

#[derive(Resource, Default)]
struct ProjectedPointerState(PointerPositionState);

#[derive(Resource, Default)]
struct ProjectedMouseButtons(HashSet<LinuxButtonCode>);

#[derive(Resource, Default)]
struct InputEffects(VecDeque<SeatInputEffect>);

#[derive(SystemParam)]
struct ProjectionMessages<'w> {
    pointer_input: MessageWriter<'w, PointerInput>,
    keyboard_input: MessageWriter<'w, KeyboardInput>,
    keyboard_focus_lost: MessageWriter<'w, KeyboardFocusLost>,
    mouse_button_input: MessageWriter<'w, MouseButtonInput>,
    mouse_motion: MessageWriter<'w, MouseMotion>,
    mouse_wheel: MessageWriter<'w, MouseWheel>,
}

#[derive(Resource, Default)]
struct InputUpdateTime(u32);

#[derive(Clone, Copy, Debug, PartialEq)]
struct ResolvedPointer {
    position: InputPosition,
    target: Option<SurfaceHit>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ImplicitPointerGrab {
    surface: SurfaceId,
    layer: SurfaceLayerId,
    origin: InputPosition,
}

impl ImplicitPointerGrab {
    fn new(position: InputPosition, hit: SurfaceHit) -> Self {
        Self {
            surface: hit.surface,
            layer: hit.layer,
            origin: InputPosition::new(
                position.x - hit.local_position.x,
                position.y - hit.local_position.y,
            ),
        }
    }

    fn target_at(self, position: InputPosition) -> SurfaceHit {
        SurfaceHit {
            surface: self.surface,
            layer: self.layer,
            local_position: InputPosition::new(
                position.x - self.origin.x,
                position.y - self.origin.y,
            ),
        }
    }
}

#[derive(Resource, Default)]
struct PointerRoutingState {
    pointer: PointerPositionState,
    last_sent: Option<ResolvedPointer>,
    pressed_buttons: HashSet<LinuxButtonCode>,
    implicit_grab: Option<ImplicitPointerGrab>,
}

pub(crate) fn enqueue_raw_input(world: &mut World, event: RawSeatEvent) {
    let Some(mut ingress) = world.get_resource_mut::<RawInputIngress>() else {
        warn!("discarded host input because the Bevy input bridge is unavailable");
        return;
    };
    ingress.0.push_back(event);
}

pub(crate) fn set_input_update_time(world: &mut World, time: u32) {
    if let Some(mut update_time) = world.get_resource_mut::<InputUpdateTime>() {
        update_time.0 = time;
    }
}

pub(crate) fn take_input_effects(world: &mut World) -> Vec<SeatInputEffect> {
    world
        .get_resource_mut::<InputEffects>()
        .map(|mut effects| effects.0.drain(..).collect())
        .unwrap_or_default()
}

fn project_raw_input(
    target: Res<InputTarget>,
    ui_scale: Res<UiScale>,
    mut ingress: ResMut<RawInputIngress>,
    mut pending: ResMut<PendingSeatInput>,
    mut projected_pointer: ResMut<ProjectedPointerState>,
    mut projected_buttons: ResMut<ProjectedMouseButtons>,
    mut messages: ProjectionMessages,
) {
    let ui_scale = ui_scale.0;
    for raw_event in ingress.0.drain(..) {
        pending.0.push_back(raw_event.clone());
        match raw_event.event {
            RawSeatEventKind::PointerMotion { position } => {
                let previous = projected_pointer.0.host_position;
                projected_pointer.0.apply(position);
                if let Some(previous) = previous {
                    // Nested mode approximates raw device motion with absolute
                    // cursor deltas. Leafwing consumes these compositor-logical
                    // units, while Bevy picking below needs UiScale-adjusted
                    // physical units for Weld's scale-1 manual target. A
                    // libinput backend can provide true device deltas.
                    messages.mouse_motion.write(MouseMotion {
                        delta: position.as_vec2() - previous.as_vec2(),
                    });
                }
                messages.pointer_input.write(pointer_motion(
                    &target.0,
                    position,
                    previous.map_or(Vec2::ZERO, |previous| {
                        position.as_vec2() - previous.as_vec2()
                    }),
                    ui_scale,
                ));
            }
            RawSeatEventKind::PointerLeft { position } => {
                projected_pointer.0.apply(position);
                projected_pointer.0.clear_host();
                messages.pointer_input.write(pointer_motion(
                    &target.0,
                    InputPosition::new(-1.0, -1.0),
                    Vec2::ZERO,
                    ui_scale,
                ));
            }
            RawSeatEventKind::PointerButton {
                position,
                button,
                state,
            } => {
                if let Some(position) = position {
                    projected_pointer.0.apply(position);
                }
                if let Some(action) = pointer_button_action(button, state)
                    && let Some(position) = projected_pointer.0.host_position
                {
                    messages.pointer_input.write(PointerInput::new(
                        PointerId::Mouse,
                        pointer_location(&target.0, position, ui_scale),
                        action,
                    ));
                }
                let linux_button = button;
                if let Some(button) = bevy_mouse_button(linux_button) {
                    match state {
                        ButtonState::Pressed => {
                            projected_buttons.0.insert(linux_button);
                        }
                        ButtonState::Released => {
                            projected_buttons.0.remove(&linux_button);
                        }
                    }
                    messages.mouse_button_input.write(MouseButtonInput {
                        button,
                        state,
                        window: INPUT_WINDOW,
                    });
                }
            }
            RawSeatEventKind::PointerAxis { position, axis } => {
                if let Some(position) = position {
                    projected_pointer.0.apply(position);
                }
                if let Some(position) = projected_pointer.0.host_position {
                    let (unit, x, y, phase) = bevy_scroll(axis);
                    messages.mouse_wheel.write(MouseWheel {
                        unit,
                        x,
                        y,
                        window: INPUT_WINDOW,
                        phase,
                    });
                    messages.pointer_input.write(PointerInput::new(
                        PointerId::Mouse,
                        pointer_location(&target.0, position, ui_scale),
                        PointerAction::Scroll { unit, x, y, phase },
                    ));
                }
            }
            RawSeatEventKind::HostFocusLost => {
                projected_pointer.0.clear_host();
                let mut held_buttons = projected_buttons.0.drain().collect::<Vec<_>>();
                held_buttons.sort_unstable_by_key(|button| button.0);
                for button in held_buttons.into_iter().filter_map(bevy_mouse_button) {
                    messages.mouse_button_input.write(MouseButtonInput {
                        button,
                        state: ButtonState::Released,
                        window: INPUT_WINDOW,
                    });
                }
                messages.keyboard_focus_lost.write(KeyboardFocusLost);
                messages.pointer_input.write(PointerInput::new(
                    PointerId::Mouse,
                    pointer_location(&target.0, projected_pointer.0.last_known_position, ui_scale),
                    PointerAction::Cancel,
                ));
                messages.pointer_input.write(pointer_motion(
                    &target.0,
                    InputPosition::new(-1.0, -1.0),
                    Vec2::ZERO,
                    ui_scale,
                ));
            }
            RawSeatEventKind::Keyboard {
                keycode,
                logical_key,
                state,
            } => {
                messages.keyboard_input.write(KeyboardInput {
                    key_code: bevy_keycode(keycode),
                    logical_key: logical_key.unwrap_or(Key::Unidentified(NativeKey::Unidentified)),
                    state,
                    text: None,
                    repeat: false,
                    window: INPUT_WINDOW,
                });
            }
        }
    }
}

fn resolve_input_effects(
    pointers: Query<(&PointerId, &PointerInteraction, &PointerLocation)>,
    picked_nodes: Query<(Option<&SurfaceInputNode>, Option<&ComputedNode>)>,
    surface_nodes: Query<(&SurfaceInputNode, &Node)>,
    update_time: Res<InputUpdateTime>,
    mut pending: ResMut<PendingSeatInput>,
    mut effects: ResMut<InputEffects>,
    mut routing: ResMut<PointerRoutingState>,
) {
    // A runtime scale change can leave picking on the previous ComputedNode
    // layout for this update; Bevy refreshes UI layout later in PostUpdate.
    // The following update reconciles a stationary pointer to the new hit.
    let geometric_target = pointers
        .iter()
        .find(|(pointer, _, _)| **pointer == PointerId::Mouse)
        .and_then(|(_, interaction, location)| {
            location.location().and_then(|_| {
                resolve_pick_candidates(interaction.iter().map(|(entity, hit)| {
                    let (surface, node) = picked_nodes.get(*entity).unwrap_or((None, None));
                    PickCandidate {
                        target: surface.copied(),
                        centered_position: hit.position.map(|position| {
                            InputPosition::new(position.x.into(), position.y.into())
                        }),
                        size: node.map(logical_pick_size),
                    }
                }))
            })
        });
    if routing.implicit_grab.is_some_and(|grab| {
        !surface_nodes.iter().any(|(surface, node)| {
            surface.surface == grab.surface
                && surface.layer == grab.layer
                && node.display != Display::None
        })
    }) {
        routing.implicit_grab = None;
    }

    // Pointer positions and grab-relative targets replay per raw event. A
    // non-grab SurfaceHit still comes from Bevy's single end-of-update pick,
    // so its local position can be sampled later than the replayed position.
    for raw_event in pending.0.drain(..) {
        if let Some(effect) = replay_seat_event(&mut routing, geometric_target, raw_event) {
            effects.0.push_back(effect);
        }
    }

    refresh_grab_origin(&mut routing, geometric_target);
    let current = resolved_pointer(&routing, geometric_target);
    if let Some(effect) = reconcile_pointer(routing.last_sent, current, update_time.0) {
        effects.0.push_back(effect);
        routing.last_sent = current;
    }
}

fn replay_seat_event(
    routing: &mut PointerRoutingState,
    geometric_target: Option<SurfaceHit>,
    raw_event: RawSeatEvent,
) -> Option<SeatInputEffect> {
    let RawSeatEvent { event, time } = raw_event;
    match event {
        RawSeatEventKind::PointerMotion { position } => {
            routing.pointer.apply(position);
            let current = resolved_pointer(routing, geometric_target)?;
            routing.last_sent = Some(current);
            Some(pointer_motion_effect(current, time))
        }
        RawSeatEventKind::PointerLeft { position } => {
            routing.pointer.apply(position);
            if routing.implicit_grab.is_some() {
                let current = resolved_pointer(routing, geometric_target)?;
                routing.last_sent = Some(current);
                Some(pointer_motion_effect(current, time))
            } else {
                routing.pointer.clear_host();
                routing.last_sent = None;
                Some(SeatInputEffect::new(
                    SeatInputEffectKind::PointerMotion {
                        position,
                        target: None,
                    },
                    time,
                ))
            }
        }
        RawSeatEventKind::PointerButton {
            position,
            button,
            state,
        } => {
            if let Some(position) = position {
                routing.pointer.apply(position);
            }
            let position = position.unwrap_or(routing.pointer.last_known_position);
            if state == ButtonState::Pressed {
                let first_button = routing.pressed_buttons.is_empty();
                routing.pressed_buttons.insert(button);
                if first_button {
                    routing.implicit_grab =
                        geometric_target.map(|target| ImplicitPointerGrab::new(position, target));
                }
            }
            let target = routed_target(routing, geometric_target, position);
            let effect = SeatInputEffect::new(
                SeatInputEffectKind::PointerButton {
                    position,
                    target,
                    button,
                    state,
                },
                time,
            );
            routing.last_sent = routing
                .pointer
                .host_position
                .map(|position| ResolvedPointer { position, target });
            if state == ButtonState::Released {
                routing.pressed_buttons.remove(&button);
                if routing.pressed_buttons.is_empty() {
                    routing.implicit_grab = None;
                }
            }
            Some(effect)
        }
        RawSeatEventKind::PointerAxis { position, axis } => {
            if let Some(position) = position {
                routing.pointer.apply(position);
            }
            let position = position.unwrap_or(routing.pointer.last_known_position);
            let target = routed_target(routing, geometric_target, position);
            trace!(?axis, ?target, "routed raw pointer axis");
            Some(SeatInputEffect::new(
                SeatInputEffectKind::PointerAxis { axis },
                time,
            ))
        }
        RawSeatEventKind::Keyboard { keycode, state, .. } => Some(SeatInputEffect::new(
            SeatInputEffectKind::Keyboard { keycode, state },
            time,
        )),
        RawSeatEventKind::HostFocusLost => {
            routing.pointer.clear_host();
            clear_pointer_routing(routing);
            Some(SeatInputEffect::new(
                SeatInputEffectKind::HostFocusLost,
                time,
            ))
        }
    }
}

fn clear_pointer_routing(routing: &mut PointerRoutingState) {
    routing.pressed_buttons.clear();
    routing.implicit_grab = None;
    routing.last_sent = None;
}

fn pointer_motion(
    target: &NormalizedRenderTarget,
    position: InputPosition,
    delta: Vec2,
    ui_scale: f32,
) -> PointerInput {
    PointerInput::new(
        PointerId::Mouse,
        pointer_location(target, position, ui_scale),
        PointerAction::Move {
            delta: delta * ui_scale,
        },
    )
}

fn pointer_location(
    target: &NormalizedRenderTarget,
    position: InputPosition,
    ui_scale: f32,
) -> Location {
    Location {
        target: target.clone(),
        // Bevy picking converts only the render target scale. Weld's manual
        // target has scale 1, so UiScale must be applied explicitly here.
        position: position.as_vec2() * ui_scale,
    }
}

fn logical_pick_size(node: &ComputedNode) -> Vec2 {
    node.size() * node.inverse_scale_factor()
}

fn pointer_button_action(button: LinuxButtonCode, state: ButtonState) -> Option<PointerAction> {
    use bevy::picking::pointer::PointerButton;

    let button = match button.0 {
        0x110 => PointerButton::Primary,
        0x111 => PointerButton::Secondary,
        0x112 => PointerButton::Middle,
        _ => return None,
    };
    Some(match state {
        ButtonState::Pressed => PointerAction::Press(button),
        ButtonState::Released => PointerAction::Release(button),
    })
}

fn bevy_mouse_button(button: LinuxButtonCode) -> Option<MouseButton> {
    match button.0 {
        0x110 => Some(MouseButton::Left),
        0x111 => Some(MouseButton::Right),
        0x112 => Some(MouseButton::Middle),
        0x113 => Some(MouseButton::Back),
        0x114 => Some(MouseButton::Forward),
        _ => None,
    }
}

fn bevy_keycode(keycode: LinuxKeycode) -> KeyCode {
    convert_physical_key_code(PhysicalKey::from_scancode(keycode.0))
}

fn bevy_scroll(axis: RawScrollFrame) -> (MouseScrollUnit, f32, f32, TouchPhase) {
    let phase = match axis.phase {
        RawScrollPhase::Started => TouchPhase::Started,
        RawScrollPhase::Moved => TouchPhase::Moved,
        RawScrollPhase::Ended => TouchPhase::Ended,
        RawScrollPhase::Cancelled => TouchPhase::Canceled,
    };
    if axis.horizontal_v120.is_some() || axis.vertical_v120.is_some() {
        (
            MouseScrollUnit::Line,
            -(axis.horizontal_v120.unwrap_or_default() as f32) / 120.0,
            -(axis.vertical_v120.unwrap_or_default() as f32) / 120.0,
            phase,
        )
    } else {
        (
            MouseScrollUnit::Pixel,
            -axis.horizontal as f32,
            -axis.vertical as f32,
            phase,
        )
    }
}

#[derive(Clone, Copy)]
struct PickCandidate {
    target: Option<SurfaceInputNode>,
    centered_position: Option<InputPosition>,
    size: Option<Vec2>,
}

/// The first interaction is authoritative with Weld's single UI picking
/// backend. Revisit this if another picking backend introduces cross-backend
/// ordering ties.
fn resolve_pick_candidates(
    candidates: impl IntoIterator<Item = PickCandidate>,
) -> Option<SurfaceHit> {
    let candidate = candidates.into_iter().next()?;
    let target = candidate.target?;
    let position = candidate.centered_position?;
    let size = candidate.size?;
    Some(SurfaceHit {
        surface: target.surface,
        layer: target.layer,
        // `size` is surface-logical because SurfacePlugin explicitly
        // sizes each node to its committed viewport destination (or the full
        // logical buffer when no viewport exists). That invariant keeps Bevy
        // transforms and clipping composable while the protocol receives
        // coordinates in the client's logical space.
        local_position: InputPosition::new(
            (position.x + 0.5) * f64::from(size.x) + f64::from(target.local_origin.x),
            (position.y + 0.5) * f64::from(size.y) + f64::from(target.local_origin.y),
        ),
    })
}

fn refresh_grab_origin(routing: &mut PointerRoutingState, geometric: Option<SurfaceHit>) {
    let (Some(position), Some(grab), Some(hit)) = (
        routing.pointer.host_position,
        routing.implicit_grab.as_mut(),
        geometric,
    ) else {
        return;
    };
    if grab.surface == hit.surface && grab.layer == hit.layer {
        *grab = ImplicitPointerGrab::new(position, hit);
    }
}

fn routed_target(
    routing: &PointerRoutingState,
    geometric: Option<SurfaceHit>,
    position: InputPosition,
) -> Option<SurfaceHit> {
    routing
        .implicit_grab
        .map(|grab| grab.target_at(position))
        .or_else(|| {
            routing
                .pressed_buttons
                .is_empty()
                .then_some(geometric)
                .flatten()
        })
}

fn resolved_pointer(
    routing: &PointerRoutingState,
    geometric: Option<SurfaceHit>,
) -> Option<ResolvedPointer> {
    routing
        .pointer
        .host_position
        .map(|position| ResolvedPointer {
            position,
            target: routed_target(routing, geometric, position),
        })
}

fn pointer_motion_effect(pointer: ResolvedPointer, time: u32) -> SeatInputEffect {
    SeatInputEffect::new(
        SeatInputEffectKind::PointerMotion {
            position: pointer.position,
            target: pointer.target,
        },
        time,
    )
}

fn reconcile_pointer(
    previous: Option<ResolvedPointer>,
    current: Option<ResolvedPointer>,
    time: u32,
) -> Option<SeatInputEffect> {
    if previous == current {
        return None;
    }
    match current {
        Some(current) => Some(pointer_motion_effect(current, time)),
        None => previous.map(|previous| {
            SeatInputEffect::new(
                SeatInputEffectKind::PointerMotion {
                    position: previous.position,
                    target: None,
                },
                time,
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::{
        camera::ManualTextureViewHandle,
        input::InputPlugin,
        prelude::{MinimalPlugins, Reflect},
    };
    use leafwing_input_manager::prelude::{ActionState, Actionlike, InputManagerPlugin, InputMap};

    #[derive(Actionlike, Clone, Copy, Debug, Eq, Hash, PartialEq, Reflect)]
    enum TestAction {
        Activate,
        Click,
    }

    fn projection_test_app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(InputPlugin)
            .add_message::<PointerInput>()
            .add_plugins(InputBridgePlugin::new(NormalizedRenderTarget::TextureView(
                ManualTextureViewHandle(1),
            )))
            .add_plugins(InputManagerPlugin::<TestAction>::default());
        let input = app
            .world_mut()
            .spawn(
                InputMap::default()
                    .with(TestAction::Activate, KeyCode::KeyF)
                    .with(TestAction::Click, MouseButton::Left),
            )
            .id();
        (app, input)
    }

    #[test]
    fn raw_keyboard_input_reaches_leafwing_in_the_same_update() {
        let (mut app, input) = projection_test_app();
        let event = RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(33),
                logical_key: Some(Key::Character("f".into())),
                state: ButtonState::Pressed,
            },
            41,
        );
        enqueue_raw_input(app.world_mut(), event.clone());

        app.update();

        let action_state = app
            .world()
            .entity(input)
            .get::<ActionState<TestAction>>()
            .expect("Leafwing should attach action state");
        assert!(action_state.pressed(&TestAction::Activate));
        assert!(action_state.just_pressed(&TestAction::Activate));
        assert_eq!(
            take_input_effects(app.world_mut()),
            [SeatInputEffect::new(
                SeatInputEffectKind::Keyboard {
                    keycode: LinuxKeycode(33),
                    state: ButtonState::Pressed,
                },
                41,
            )]
        );
    }

    #[test]
    fn raw_event_order_and_timestamps_survive_projection() {
        let (mut app, _) = projection_test_app();
        let events = [
            RawSeatEvent::new(
                RawSeatEventKind::PointerMotion {
                    position: InputPosition::new(12.0, 34.0),
                },
                7,
            ),
            RawSeatEvent::new(
                RawSeatEventKind::PointerButton {
                    position: Some(InputPosition::new(12.0, 34.0)),
                    button: LinuxButtonCode(0x110),
                    state: ButtonState::Pressed,
                },
                8,
            ),
            RawSeatEvent::new(RawSeatEventKind::HostFocusLost, 9),
        ];
        for event in events.iter().cloned() {
            enqueue_raw_input(app.world_mut(), event);
        }

        app.update();

        assert_eq!(
            take_input_effects(app.world_mut()),
            [
                SeatInputEffect::new(
                    SeatInputEffectKind::PointerMotion {
                        position: InputPosition::new(12.0, 34.0),
                        target: None,
                    },
                    7,
                ),
                SeatInputEffect::new(
                    SeatInputEffectKind::PointerButton {
                        position: InputPosition::new(12.0, 34.0),
                        target: None,
                        button: LinuxButtonCode(0x110),
                        state: ButtonState::Pressed,
                    },
                    8,
                ),
                SeatInputEffect::new(SeatInputEffectKind::HostFocusLost, 9),
            ]
        );
    }

    #[test]
    fn host_focus_loss_releases_leafwing_and_pointer_grabs() {
        let (mut app, input) = projection_test_app();
        enqueue_raw_input(
            app.world_mut(),
            RawSeatEvent::new(
                RawSeatEventKind::Keyboard {
                    keycode: LinuxKeycode(33),
                    logical_key: Some(Key::Character("f".into())),
                    state: ButtonState::Pressed,
                },
                1,
            ),
        );
        enqueue_raw_input(
            app.world_mut(),
            RawSeatEvent::new(
                RawSeatEventKind::PointerButton {
                    position: Some(InputPosition::new(10.0, 10.0)),
                    button: LinuxButtonCode(0x110),
                    state: ButtonState::Pressed,
                },
                1,
            ),
        );
        app.update();
        enqueue_raw_input(
            app.world_mut(),
            RawSeatEvent::new(RawSeatEventKind::HostFocusLost, 2),
        );
        app.update();
        let action_state = app
            .world()
            .entity(input)
            .get::<ActionState<TestAction>>()
            .expect("Leafwing should attach action state");
        assert!(!action_state.pressed(&TestAction::Activate));
        assert!(!action_state.pressed(&TestAction::Click));
        assert!(
            !app.world()
                .resource::<bevy::input::ButtonInput<MouseButton>>()
                .pressed(MouseButton::Left)
        );

        let surface = SurfaceId::new(3);
        let mut routing = PointerRoutingState {
            last_sent: Some(ResolvedPointer {
                position: InputPosition::new(4.0, 5.0),
                target: None,
            }),
            implicit_grab: Some(ImplicitPointerGrab {
                surface,
                layer: SurfaceLayerId::new(1),
                origin: InputPosition::default(),
            }),
            ..Default::default()
        };
        routing.pressed_buttons.insert(LinuxButtonCode(0x110));
        clear_pointer_routing(&mut routing);
        assert!(routing.pressed_buttons.is_empty());
        assert_eq!(routing.implicit_grab, None);
        assert_eq!(routing.last_sent, None);
    }

    fn surface_candidate(
        surface: SurfaceId,
        centered_position: InputPosition,
        size: Vec2,
    ) -> PickCandidate {
        PickCandidate {
            target: Some(SurfaceInputNode {
                surface,
                layer: SurfaceLayerId::new(1),
                local_origin: Vec2::ZERO,
            }),
            centered_position: Some(centered_position),
            size: Some(size),
        }
    }

    #[test]
    fn topmost_surface_pick_is_authoritative() {
        let top = SurfaceId::new(1);
        let lower = SurfaceId::new(2);
        let hit = resolve_pick_candidates([
            surface_candidate(top, InputPosition::default(), Vec2::splat(100.0)),
            surface_candidate(lower, InputPosition::default(), Vec2::splat(100.0)),
        ])
        .expect("top surface should be routed");

        assert_eq!(hit.surface, top);
    }

    #[test]
    fn topmost_overlay_blocks_the_surface_below() {
        let overlay = PickCandidate {
            target: None,
            centered_position: Some(InputPosition::default()),
            size: Some(Vec2::splat(100.0)),
        };
        let surface = surface_candidate(
            SurfaceId::new(1),
            InputPosition::default(),
            Vec2::splat(100.0),
        );

        assert_eq!(resolve_pick_candidates([overlay, surface]), None);
    }

    #[test]
    fn centered_ui_coordinates_convert_to_surface_pixels() {
        let surface = SurfaceId::new(1);
        let top_left = resolve_pick_candidates([surface_candidate(
            surface,
            InputPosition::new(-0.5, -0.5),
            Vec2::new(640.0, 480.0),
        )])
        .expect("corner should be inside the picked surface");
        let transformed = resolve_pick_candidates([surface_candidate(
            surface,
            InputPosition::new(0.25, -0.25),
            Vec2::new(640.0, 480.0),
        )])
        .expect("transformed local hit should be routed");
        let bottom_right = resolve_pick_candidates([surface_candidate(
            surface,
            InputPosition::new(0.5, 0.5),
            Vec2::new(640.0, 480.0),
        )])
        .expect("corner should be inside the picked surface");

        assert_eq!(top_left.local_position, InputPosition::new(0.0, 0.0));
        assert_eq!(transformed.local_position, InputPosition::new(480.0, 120.0));
        assert_eq!(
            bottom_right.local_position,
            InputPosition::new(640.0, 480.0)
        );
    }

    #[test]
    fn input_region_origin_is_restored_for_client_pointer_coordinates() {
        let surface = SurfaceId::new(1);
        let hit = resolve_pick_candidates([PickCandidate {
            target: Some(SurfaceInputNode {
                surface,
                layer: SurfaceLayerId::new(1),
                local_origin: Vec2::new(24.0, 32.0),
            }),
            centered_position: Some(InputPosition::new(-0.5, -0.5)),
            size: Some(Vec2::new(640.0, 480.0)),
        }])
        .expect("window geometry should remain pickable");

        assert_eq!(hit.local_position, InputPosition::new(24.0, 32.0));
    }

    #[test]
    fn compositor_logical_pointer_positions_are_scaled_for_bevy_picking() {
        let target = NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1));
        let location = pointer_location(&target, InputPosition::new(80.0, 40.0), 1.25);

        assert_eq!(location.position, Vec2::new(100.0, 50.0));
    }

    #[test]
    fn physical_computed_node_size_becomes_surface_logical_for_hits() {
        let node = ComputedNode {
            size: Vec2::new(800.0, 600.0),
            // Reciprocal of the host scale 1.25 used by this scaling slice.
            inverse_scale_factor: 0.8,
            ..Default::default()
        };

        assert_eq!(logical_pick_size(&node), Vec2::new(640.0, 480.0));
    }

    #[test]
    fn losing_the_last_hit_emits_pointer_focus_clear() {
        let previous = ResolvedPointer {
            position: InputPosition::new(42.0, 24.0),
            target: Some(SurfaceHit {
                surface: SurfaceId::new(1),
                layer: SurfaceLayerId::new(1),
                local_position: InputPosition::new(2.0, 3.0),
            }),
        };

        assert_eq!(
            reconcile_pointer(Some(previous), None, 9),
            Some(SeatInputEffect::new(
                SeatInputEffectKind::PointerMotion {
                    position: previous.position,
                    target: None,
                },
                9,
            ))
        );
    }

    #[test]
    fn unchanged_pointer_does_not_emit_reconciliation_motion() {
        let current = ResolvedPointer {
            position: InputPosition::new(5.0, 6.0),
            target: None,
        };

        assert_eq!(reconcile_pointer(Some(current), Some(current), 10), None);
    }

    #[test]
    fn stationary_pointer_entering_a_surface_emits_motion() {
        let current = ResolvedPointer {
            position: InputPosition::new(50.0, 60.0),
            target: Some(SurfaceHit {
                surface: SurfaceId::new(2),
                layer: SurfaceLayerId::new(1),
                local_position: InputPosition::new(10.0, 20.0),
            }),
        };

        assert_eq!(
            reconcile_pointer(None, Some(current), 11),
            Some(pointer_motion_effect(current, 11))
        );
    }

    #[test]
    fn implicit_grab_keeps_routing_outside_the_surface() {
        let surface = SurfaceId::new(7);
        let mut routing = PointerRoutingState {
            pointer: PointerPositionState {
                host_position: Some(InputPosition::new(90.0, 100.0)),
                last_known_position: InputPosition::new(90.0, 100.0),
            },
            implicit_grab: Some(ImplicitPointerGrab::new(
                InputPosition::new(30.0, 40.0),
                SurfaceHit {
                    surface,
                    layer: SurfaceLayerId::new(1),
                    local_position: InputPosition::new(10.0, 15.0),
                },
            )),
            ..Default::default()
        };
        routing.pressed_buttons.insert(LinuxButtonCode(0x110));

        assert_eq!(
            routed_target(&routing, None, routing.pointer.host_position.unwrap()),
            Some(SurfaceHit {
                surface,
                layer: SurfaceLayerId::new(1),
                local_position: InputPosition::new(70.0, 75.0),
            })
        );
    }

    #[test]
    fn press_without_surface_grab_swallows_client_routing() {
        let mut routing = PointerRoutingState::default();
        routing.pressed_buttons.insert(LinuxButtonCode(0x110));
        let geometric = SurfaceHit {
            surface: SurfaceId::new(9),
            layer: SurfaceLayerId::new(1),
            local_position: InputPosition::new(1.0, 2.0),
        };

        assert_eq!(
            routed_target(&routing, Some(geometric), InputPosition::new(5.0, 6.0)),
            None
        );
    }

    #[test]
    fn final_grab_release_reconciles_to_pointer_leave() {
        let position = InputPosition::new(90.0, 100.0);
        let grabbed = ResolvedPointer {
            position,
            target: Some(SurfaceHit {
                surface: SurfaceId::new(10),
                layer: SurfaceLayerId::new(1),
                local_position: InputPosition::new(70.0, 75.0),
            }),
        };
        let routing = PointerRoutingState {
            pointer: PointerPositionState {
                host_position: Some(position),
                last_known_position: position,
            },
            ..Default::default()
        };
        let current = resolved_pointer(&routing, None);

        assert_eq!(
            reconcile_pointer(Some(grabbed), current, 12),
            Some(SeatInputEffect::new(
                SeatInputEffectKind::PointerMotion {
                    position,
                    target: None,
                },
                12,
            ))
        );
    }

    #[test]
    fn grabbed_motion_batch_replays_each_position_before_reanchoring() {
        let surface = SurfaceId::new(11);
        let initial_position = InputPosition::new(10.0, 10.0);
        let mut routing = PointerRoutingState {
            pointer: PointerPositionState {
                host_position: Some(initial_position),
                last_known_position: initial_position,
            },
            implicit_grab: Some(ImplicitPointerGrab::new(
                initial_position,
                SurfaceHit {
                    surface,
                    layer: SurfaceLayerId::new(1),
                    local_position: InputPosition::new(2.0, 3.0),
                },
            )),
            ..Default::default()
        };
        routing.pressed_buttons.insert(LinuxButtonCode(0x110));
        let first_position = InputPosition::new(20.0, 20.0);
        let final_position = InputPosition::new(30.0, 25.0);

        let effects = [
            replay_seat_event(
                &mut routing,
                None,
                RawSeatEvent::new(
                    RawSeatEventKind::PointerMotion {
                        position: first_position,
                    },
                    2,
                ),
            )
            .expect("grabbed motion should route"),
            replay_seat_event(
                &mut routing,
                None,
                RawSeatEvent::new(
                    RawSeatEventKind::PointerMotion {
                        position: final_position,
                    },
                    3,
                ),
            )
            .expect("grabbed motion should route"),
        ];

        assert_eq!(
            effects,
            [
                SeatInputEffect::new(
                    SeatInputEffectKind::PointerMotion {
                        position: first_position,
                        target: Some(SurfaceHit {
                            surface,
                            layer: SurfaceLayerId::new(1),
                            local_position: InputPosition::new(12.0, 13.0),
                        }),
                    },
                    2,
                ),
                SeatInputEffect::new(
                    SeatInputEffectKind::PointerMotion {
                        position: final_position,
                        target: Some(SurfaceHit {
                            surface,
                            layer: SurfaceLayerId::new(1),
                            local_position: InputPosition::new(22.0, 18.0),
                        }),
                    },
                    3,
                ),
            ]
        );

        let final_hit = SurfaceHit {
            surface,
            layer: SurfaceLayerId::new(1),
            local_position: InputPosition::new(40.0, 50.0),
        };
        refresh_grab_origin(&mut routing, Some(final_hit));
        assert_eq!(
            routing
                .implicit_grab
                .expect("grab should remain active")
                .target_at(final_position),
            final_hit
        );
    }

    #[test]
    fn agreeing_geometric_hit_refreshes_grab_origin() {
        let surface = SurfaceId::new(8);
        let mut routing = PointerRoutingState {
            pointer: PointerPositionState {
                host_position: Some(InputPosition::new(80.0, 90.0)),
                last_known_position: InputPosition::new(80.0, 90.0),
            },
            implicit_grab: Some(ImplicitPointerGrab {
                surface,
                layer: SurfaceLayerId::new(1),
                origin: InputPosition::default(),
            }),
            ..Default::default()
        };
        let hit = SurfaceHit {
            surface,
            layer: SurfaceLayerId::new(1),
            local_position: InputPosition::new(20.0, 30.0),
        };

        refresh_grab_origin(&mut routing, Some(hit));

        assert_eq!(
            routing
                .implicit_grab
                .unwrap()
                .target_at(routing.pointer.host_position.unwrap()),
            hit
        );
    }

    #[test]
    fn pointer_axis_does_not_absorb_a_pending_focus_reconciliation() {
        let position = InputPosition::new(80.0, 90.0);
        let previous = ResolvedPointer {
            position,
            target: Some(SurfaceHit {
                surface: SurfaceId::new(1),
                layer: SurfaceLayerId::new(1),
                local_position: InputPosition::new(10.0, 20.0),
            }),
        };
        let next = SurfaceHit {
            surface: SurfaceId::new(2),
            layer: SurfaceLayerId::new(1),
            local_position: InputPosition::new(30.0, 40.0),
        };
        let mut routing = PointerRoutingState {
            pointer: PointerPositionState {
                host_position: Some(position),
                last_known_position: position,
            },
            last_sent: Some(previous),
            ..Default::default()
        };

        let effect = replay_seat_event(
            &mut routing,
            Some(next),
            RawSeatEvent::new(
                RawSeatEventKind::PointerAxis {
                    position: Some(position),
                    axis: RawScrollFrame {
                        source: crate::raw_input::RawScrollSource::Wheel,
                        phase: RawScrollPhase::Moved,
                        horizontal: 0.0,
                        vertical: 15.0,
                        horizontal_v120: None,
                        vertical_v120: Some(120),
                        horizontal_stop: false,
                        vertical_stop: false,
                    },
                },
                11,
            ),
        );

        assert!(matches!(
            effect,
            Some(SeatInputEffect {
                event: SeatInputEffectKind::PointerAxis { .. },
                time: 11,
            })
        ));
        assert_eq!(routing.last_sent, Some(previous));
        assert_eq!(
            reconcile_pointer(
                routing.last_sent,
                resolved_pointer(&routing, Some(next)),
                11
            ),
            Some(pointer_motion_effect(
                ResolvedPointer {
                    position,
                    target: Some(next),
                },
                11,
            ))
        );
    }

    #[test]
    fn bevy_scroll_reverses_wayland_axes_and_scales_v120() {
        let wheel = RawScrollFrame {
            source: crate::raw_input::RawScrollSource::Wheel,
            phase: RawScrollPhase::Moved,
            horizontal: -30.0,
            vertical: 45.0,
            horizontal_v120: Some(-240),
            vertical_v120: Some(360),
            horizontal_stop: false,
            vertical_stop: false,
        };
        let continuous = RawScrollFrame {
            source: crate::raw_input::RawScrollSource::Continuous,
            phase: RawScrollPhase::Started,
            horizontal: -4.5,
            vertical: 2.5,
            horizontal_v120: None,
            vertical_v120: None,
            horizontal_stop: false,
            vertical_stop: false,
        };

        assert_eq!(
            bevy_scroll(wheel),
            (MouseScrollUnit::Line, 2.0, -3.0, TouchPhase::Moved)
        );
        assert_eq!(
            bevy_scroll(continuous),
            (MouseScrollUnit::Pixel, 4.5, -2.5, TouchPhase::Started,)
        );
    }
}
