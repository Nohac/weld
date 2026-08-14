//! Same-update projection from raw input into Bevy and Leafwing.

use std::collections::{HashSet, VecDeque};

use bevy::{
    app::{App, First},
    camera::NormalizedRenderTarget,
    ecs::{
        message::{Message, MessageUpdateSystems, MessageWriter},
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Res, ResMut, SystemParam},
        world::World,
    },
    input::{
        ButtonState as BevyButtonState,
        keyboard::{Key, KeyCode, KeyboardFocusLost, KeyboardInput, NativeKey},
        mouse::{MouseButton, MouseButtonInput, MouseMotion, MouseScrollUnit, MouseWheel},
        touch::TouchPhase,
    },
    math::Vec2,
    picking::pointer::{Location, PointerAction, PointerId, PointerInput},
    ui::UiScale,
};
use bevy_winit::converters::{convert_logical_key, convert_physical_key_code};
use leafwing_input_manager::plugin::CentralInputStorePlugin;
use tracing::warn;
use winit::{keyboard::PhysicalKey, platform::scancode::PhysicalKeyExtScancode};

use super::{
    raw::{
        ButtonState, InputPosition, LinuxButtonCode, LinuxKeycode, PointerGesture, RawScrollFrame,
        RawScrollPhase, RawSeatEvent, RawSeatEventKind,
    },
    state::{PendingSeatInput, ProjectedPointerState},
};

// Weld has no Bevy Window entity: the manual render target is not a window.
// Bevy input state systems and Leafwing intentionally ignore this field.
const INPUT_WINDOW: bevy::ecs::entity::Entity = bevy::ecs::entity::Entity::PLACEHOLDER;

pub(super) fn register(app: &mut App, target: NormalizedRenderTarget) {
    if !app.is_plugin_added::<CentralInputStorePlugin>() {
        app.add_plugins(CentralInputStorePlugin);
    }
    app.insert_resource(InputTarget(target))
        .add_message::<TouchpadGesture>()
        .init_resource::<UiScale>()
        .init_resource::<RawInputIngress>()
        .init_resource::<ProjectedMouseButtons>()
        .add_systems(First, project_raw_input.after(MessageUpdateSystems));
}

/// One full-fidelity touchpad gesture transition available to Weld plugins.
#[derive(Clone, Copy, Debug, Message, PartialEq)]
pub struct TouchpadGesture {
    pub gesture: PointerGesture,
    pub time: u32,
}

impl TouchpadGesture {
    pub const fn new(gesture: PointerGesture, time: u32) -> Self {
        Self { gesture, time }
    }
}

#[derive(Resource)]
struct InputTarget(NormalizedRenderTarget);

#[derive(Resource, Default)]
struct RawInputIngress(VecDeque<RawSeatEvent>);

#[derive(Resource, Default)]
struct ProjectedMouseButtons(HashSet<LinuxButtonCode>);

#[derive(SystemParam)]
struct ProjectionMessages<'w> {
    pointer_input: MessageWriter<'w, PointerInput>,
    keyboard_input: MessageWriter<'w, KeyboardInput>,
    keyboard_focus_lost: MessageWriter<'w, KeyboardFocusLost>,
    mouse_button_input: MessageWriter<'w, MouseButtonInput>,
    mouse_motion: MessageWriter<'w, MouseMotion>,
    mouse_wheel: MessageWriter<'w, MouseWheel>,
    touchpad_gesture: MessageWriter<'w, TouchpadGesture>,
}

pub(crate) fn enqueue_raw_input(world: &mut World, event: RawSeatEvent) {
    let Some(mut ingress) = world.get_resource_mut::<RawInputIngress>() else {
        warn!("discarded host input because the Bevy input bridge is unavailable");
        return;
    };
    ingress.0.push_back(event);
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
                        delta: input_vec2(position) - input_vec2(previous),
                    });
                }
                messages.pointer_input.write(pointer_motion(
                    &target.0,
                    position,
                    previous.map_or(Vec2::ZERO, |previous| {
                        input_vec2(position) - input_vec2(previous)
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
                        state: bevy_button_state(state),
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
            RawSeatEventKind::PointerGesture { gesture } => {
                messages
                    .touchpad_gesture
                    .write(TouchpadGesture::new(gesture, raw_event.time));
            }
            RawSeatEventKind::HostFocusLost => {
                projected_pointer.0.clear_host();
                let mut held_buttons = projected_buttons.0.drain().collect::<Vec<_>>();
                held_buttons.sort_unstable_by_key(|button| button.0);
                for button in held_buttons.into_iter().filter_map(bevy_mouse_button) {
                    messages.mouse_button_input.write(MouseButtonInput {
                        button,
                        state: BevyButtonState::Released,
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
                    logical_key: logical_key
                        .as_ref()
                        .map(convert_logical_key)
                        .unwrap_or(Key::Unidentified(NativeKey::Unidentified)),
                    state: bevy_button_state(state),
                    text: None,
                    repeat: false,
                    window: INPUT_WINDOW,
                });
            }
        }
    }
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
        position: input_vec2(position) * ui_scale,
    }
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

const fn bevy_button_state(state: ButtonState) -> BevyButtonState {
    match state {
        ButtonState::Pressed => BevyButtonState::Pressed,
        ButtonState::Released => BevyButtonState::Released,
    }
}

fn input_vec2(position: InputPosition) -> Vec2 {
    Vec2::new(position.x as f32, position.y as f32)
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

#[cfg(test)]
mod tests {
    use bevy::{
        camera::{ManualTextureViewHandle, NormalizedRenderTarget},
        input::{mouse::MouseScrollUnit, touch::TouchPhase},
        math::Vec2,
    };

    use super::{bevy_scroll, pointer_location};
    use crate::input::raw::{InputPosition, RawScrollFrame, RawScrollPhase};

    #[test]
    fn compositor_logical_pointer_positions_are_scaled_for_bevy_picking() {
        let target = NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1));
        let location = pointer_location(&target, InputPosition::new(80.0, 40.0), 1.25);

        assert_eq!(location.position, Vec2::new(100.0, 50.0));
    }

    #[test]
    fn bevy_scroll_reverses_wayland_axes_and_scales_v120() {
        let wheel = RawScrollFrame {
            source: crate::input::raw::RawScrollSource::Wheel,
            phase: RawScrollPhase::Moved,
            horizontal: -30.0,
            vertical: 45.0,
            horizontal_v120: Some(-240),
            vertical_v120: Some(360),
            horizontal_stop: false,
            vertical_stop: false,
        };
        let continuous = RawScrollFrame {
            source: crate::input::raw::RawScrollSource::Continuous,
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
