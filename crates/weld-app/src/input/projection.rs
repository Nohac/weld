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
};
use bevy_winit::converters::{convert_logical_key, convert_physical_key_code};
use leafwing_input_manager::plugin::CentralInputStorePlugin;
use tracing::warn;
use winit::{keyboard::PhysicalKey, platform::scancode::PhysicalKeyExtScancode};

use super::{
    InputOutputTarget,
    ingress::{ApplicationInputBuffer, INPUT_BURST_CAPACITY},
    raw::{
        ButtonState, InputPosition, LinuxButtonCode, LinuxKeycode, PointerGesture, RawScrollFrame,
        RawScrollPhase, RawSeatEvent, RawSeatEventKind,
    },
    state::{PendingSeatInput, ProjectedPointerState},
};

// Weld has no Bevy Window entity: the manual render target is not a window.
// Bevy input state systems and Leafwing intentionally ignore this field.
const INPUT_WINDOW: bevy::ecs::entity::Entity = bevy::ecs::entity::Entity::PLACEHOLDER;

pub(super) fn register(app: &mut App, targets: Vec<InputOutputTarget>) {
    if !app.is_plugin_added::<CentralInputStorePlugin>() {
        app.add_plugins(CentralInputStorePlugin);
    }
    app.insert_resource(InputTargets(targets))
        .add_message::<TouchpadGesture>()
        .init_resource::<RawInputIngress>()
        .init_resource::<ProjectedMouseButtons>()
        .init_resource::<CapturedPointerTarget>()
        .add_systems(First, project_raw_input.after(MessageUpdateSystems));
    super::pointer_shortcuts::register(app);
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
struct InputTargets(Vec<InputOutputTarget>);

impl InputTargets {
    fn project(&self, position: InputPosition) -> Option<(&NormalizedRenderTarget, Vec2)> {
        let index = self.output_at(position)?;
        self.project_to(index, position)
    }

    fn output_at(&self, position: InputPosition) -> Option<usize> {
        self.0.iter().position(|output| {
            output
                .configuration
                .logical_rect()
                .contains(position.x, position.y)
        })
    }

    fn project_to(
        &self,
        index: usize,
        position: InputPosition,
    ) -> Option<(&NormalizedRenderTarget, Vec2)> {
        let output = self.0.get(index)?;
        let origin = output.configuration.position();
        Some((
            &output.target,
            Vec2::new(position.x as f32 - origin.x, position.y as f32 - origin.y),
        ))
    }

    fn fallback(&self) -> Option<&NormalizedRenderTarget> {
        self.0
            .iter()
            .find(|output| output.configuration.is_primary())
            .or_else(|| self.0.first())
            .map(|output| &output.target)
    }

    fn update_configurations(&mut self, configurations: &[weld_core::OutputConfiguration]) {
        for target in &mut self.0 {
            if let Some(configuration) = configurations
                .iter()
                .find(|configuration| configuration.id() == target.configuration.id())
            {
                target.configuration = *configuration;
            }
        }
    }
}

#[derive(Resource)]
struct RawInputIngress(VecDeque<RawSeatEvent>);

impl Default for RawInputIngress {
    fn default() -> Self {
        Self(VecDeque::with_capacity(INPUT_BURST_CAPACITY))
    }
}

#[derive(Resource, Default)]
struct ProjectedMouseButtons(HashSet<LinuxButtonCode>);

#[derive(Resource, Default)]
struct CapturedPointerTarget {
    output: Option<usize>,
    buttons: HashSet<LinuxButtonCode>,
}

impl CapturedPointerTarget {
    fn project<'a>(
        &self,
        targets: &'a InputTargets,
        position: InputPosition,
    ) -> Option<(&'a NormalizedRenderTarget, Vec2)> {
        self.output
            .and_then(|output| targets.project_to(output, position))
            .or_else(|| targets.project(position))
    }

    fn press(&mut self, targets: &InputTargets, position: InputPosition, button: LinuxButtonCode) {
        if self.buttons.is_empty() {
            self.output = targets.output_at(position);
        }
        self.buttons.insert(button);
    }

    fn release(&mut self, button: LinuxButtonCode) -> bool {
        let removed = self.buttons.remove(&button);
        if removed && self.buttons.is_empty() {
            self.output = None;
            return true;
        }
        false
    }

    fn clear(&mut self) {
        self.buttons.clear();
        self.output = None;
    }
}

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

#[cfg(test)]
pub(crate) fn enqueue_raw_input(world: &mut World, event: RawSeatEvent) {
    let mut events = VecDeque::from([event]);
    enqueue_raw_input_batch(world, &mut events);
}

pub(crate) fn enqueue_raw_input_batch(world: &mut World, events: &mut VecDeque<RawSeatEvent>) {
    let Some(mut ingress) = world.get_resource_mut::<RawInputIngress>() else {
        warn!("discarded host input because the Bevy input bridge is unavailable");
        return;
    };
    ingress.0.append(events);
}

pub(crate) fn enqueue_application_input_batch(
    world: &mut World,
    events: &mut ApplicationInputBuffer,
) {
    enqueue_raw_input_batch(world, events.events_mut());
}

pub(crate) fn update_output_configurations(
    world: &mut World,
    configurations: &[weld_core::OutputConfiguration],
) {
    if let Some(mut targets) = world.get_resource_mut::<InputTargets>() {
        targets.update_configurations(configurations);
    }
}

fn project_raw_input(
    targets: Res<InputTargets>,
    mut ingress: ResMut<RawInputIngress>,
    mut pending: ResMut<PendingSeatInput>,
    mut projected_pointer: ResMut<ProjectedPointerState>,
    mut projected_buttons: ResMut<ProjectedMouseButtons>,
    mut captured_target: ResMut<CapturedPointerTarget>,
    mut messages: ProjectionMessages,
) {
    while let Some(raw_event) = ingress.0.pop_front() {
        match &raw_event.event {
            RawSeatEventKind::PointerMotion { position } => {
                let position = *position;
                let previous = projected_pointer.0.host_position;
                projected_pointer.0.apply(position);
                if let Some(previous) = previous {
                    // Nested mode approximates raw device motion with absolute
                    // cursor deltas. Leafwing consumes compositor-logical
                    // units, while Bevy picking receives coordinates local to
                    // the selected output target. A libinput backend can
                    // provide true device deltas.
                    messages.mouse_motion.write(MouseMotion {
                        delta: input_vec2(position) - input_vec2(previous),
                    });
                }
                if let Some((target, local_position)) = captured_target.project(&targets, position)
                {
                    messages.pointer_input.write(pointer_motion(
                        target,
                        local_position,
                        previous.map_or(Vec2::ZERO, |previous| {
                            input_vec2(position) - input_vec2(previous)
                        }),
                    ));
                }
            }
            RawSeatEventKind::PointerLeft { position } => {
                let position = *position;
                projected_pointer.0.apply(position);
                projected_pointer.0.clear_host();
                captured_target.clear();
                if let Some(target) = targets.fallback() {
                    messages.pointer_input.write(pointer_motion(
                        target,
                        Vec2::new(-1.0, -1.0),
                        Vec2::ZERO,
                    ));
                }
            }
            RawSeatEventKind::PointerButton {
                position,
                button,
                state,
            } => {
                if let Some(position) = *position {
                    projected_pointer.0.apply(position);
                }
                if let Some(position) = projected_pointer.0.host_position {
                    for input in project_pointer_button(
                        &mut captured_target,
                        &targets,
                        position,
                        *button,
                        *state,
                    )
                    .into_iter()
                    .flatten()
                    {
                        messages.pointer_input.write(input);
                    }
                }
                let linux_button = *button;
                if let Some(button) = bevy_mouse_button(linux_button) {
                    match *state {
                        ButtonState::Pressed => {
                            projected_buttons.0.insert(linux_button);
                        }
                        ButtonState::Released => {
                            projected_buttons.0.remove(&linux_button);
                        }
                    }
                    messages.mouse_button_input.write(MouseButtonInput {
                        button,
                        state: bevy_button_state(*state),
                        window: INPUT_WINDOW,
                    });
                }
            }
            RawSeatEventKind::PointerAxis { position, axis } => {
                if let Some(position) = *position {
                    projected_pointer.0.apply(position);
                }
                if let Some(position) = projected_pointer.0.host_position
                    && let Some((target, local_position)) =
                        captured_target.project(&targets, position)
                {
                    let (unit, x, y, phase) = bevy_scroll(*axis);
                    messages.mouse_wheel.write(MouseWheel {
                        unit,
                        x,
                        y,
                        window: INPUT_WINDOW,
                        phase,
                    });
                    messages.pointer_input.write(PointerInput::new(
                        PointerId::Mouse,
                        pointer_location(target, local_position),
                        PointerAction::Scroll { unit, x, y, phase },
                    ));
                }
            }
            RawSeatEventKind::PointerGesture { gesture } => {
                messages
                    .touchpad_gesture
                    .write(TouchpadGesture::new(*gesture, raw_event.time));
            }
            RawSeatEventKind::HostFocusLost => {
                projected_pointer.0.clear_host();
                captured_target.clear();
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
                if let Some((target, local_position)) =
                    targets.project(projected_pointer.0.last_known_position)
                {
                    messages.pointer_input.write(PointerInput::new(
                        PointerId::Mouse,
                        pointer_location(target, local_position),
                        PointerAction::Cancel,
                    ));
                    messages.pointer_input.write(pointer_motion(
                        target,
                        Vec2::new(-1.0, -1.0),
                        Vec2::ZERO,
                    ));
                }
            }
            RawSeatEventKind::Keyboard {
                keycode,
                logical_key,
                state,
            } => {
                messages.keyboard_input.write(KeyboardInput {
                    key_code: bevy_keycode(*keycode),
                    logical_key: logical_key
                        .as_ref()
                        .map(convert_logical_key)
                        .unwrap_or(Key::Unidentified(NativeKey::Unidentified)),
                    state: bevy_button_state(*state),
                    text: None,
                    repeat: false,
                    window: INPUT_WINDOW,
                });
            }
        }
        pending.0.push_back(raw_event);
    }
}

fn pointer_motion(target: &NormalizedRenderTarget, position: Vec2, delta: Vec2) -> PointerInput {
    PointerInput::new(
        PointerId::Mouse,
        pointer_location(target, position),
        PointerAction::Move { delta },
    )
}

fn pointer_location(target: &NormalizedRenderTarget, position: Vec2) -> Location {
    Location {
        target: target.clone(),
        position,
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

fn project_pointer_button(
    capture: &mut CapturedPointerTarget,
    targets: &InputTargets,
    position: InputPosition,
    button: LinuxButtonCode,
    state: ButtonState,
) -> [Option<PointerInput>; 2] {
    let Some(action) = pointer_button_action(button, state) else {
        return [None, None];
    };
    if state == ButtonState::Pressed {
        capture.press(targets, position, button);
    }
    let action = capture.project(targets, position).map(|(target, local)| {
        PointerInput::new(PointerId::Mouse, pointer_location(target, local), action)
    });
    let handoff = (state == ButtonState::Released && capture.release(button))
        .then(|| targets.project(position))
        .flatten()
        .map(|(target, local)| pointer_motion(target, local, Vec2::ZERO));
    [action, handoff]
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
        picking::pointer::{PointerAction, PointerButton},
    };
    use weld_core::{
        OutputConfiguration, OutputId, OutputScale,
        surface::{Extent, LogicalPoint},
    };

    use super::{
        CapturedPointerTarget, InputTargets, bevy_scroll, pointer_location, project_pointer_button,
    };
    use crate::input::{
        InputOutputTarget,
        raw::{ButtonState, InputPosition, LinuxButtonCode, RawScrollFrame, RawScrollPhase},
    };

    fn stacked_targets() -> (InputTargets, NormalizedRenderTarget, NormalizedRenderTarget) {
        let external = NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1));
        let laptop = NormalizedRenderTarget::TextureView(ManualTextureViewHandle(2));
        let targets = InputTargets(vec![
            InputOutputTarget {
                configuration: OutputConfiguration::new(
                    OutputId::new(1),
                    Extent::new(1920, 1080),
                    OutputScale::new(1.0).expect("test scale should be valid"),
                    LogicalPoint::new(0.0, 0.0),
                    false,
                    None,
                )
                .expect("external output should be valid"),
                target: external.clone(),
            },
            InputOutputTarget {
                configuration: OutputConfiguration::new(
                    OutputId::new(2),
                    Extent::new(1920, 1200),
                    OutputScale::new(1.25).expect("test scale should be valid"),
                    LogicalPoint::new(0.0, 1080.0),
                    true,
                    None,
                )
                .expect("laptop output should be valid"),
                target: laptop.clone(),
            },
        ]);
        (targets, external, laptop)
    }

    #[test]
    fn bevy_picking_locations_remain_output_logical() {
        let target = NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1));
        let location = pointer_location(&target, Vec2::new(80.0, 40.0));

        assert_eq!(location.position, Vec2::new(80.0, 40.0));
    }

    #[test]
    fn held_pointer_target_keeps_cross_output_drag_coordinates_continuous() {
        let (targets, _, laptop_target) = stacked_targets();
        let mut capture = CapturedPointerTarget::default();
        let press = InputPosition::new(100.0, 1085.0);
        capture.press(&targets, press, LinuxButtonCode(0x110));

        let (press_target, press_local) = capture
            .project(&targets, press)
            .expect("pressed pointer should project");
        let (moved_target, moved_local) = capture
            .project(&targets, InputPosition::new(100.0, 1075.0))
            .expect("captured pointer should project beyond its output");

        assert_eq!(press_target, &laptop_target);
        assert_eq!(moved_target, &laptop_target);
        assert_eq!(moved_local - press_local, Vec2::new(0.0, -10.0));
    }

    #[test]
    fn final_release_hands_the_stationary_pointer_to_its_current_output() {
        let (targets, external_target, laptop_target) = stacked_targets();
        let mut capture = CapturedPointerTarget::default();
        let primary = LinuxButtonCode(0x110);
        let press = InputPosition::new(100.0, 1085.0);
        let release = InputPosition::new(100.0, 1075.0);
        project_pointer_button(&mut capture, &targets, press, primary, ButtonState::Pressed);

        let [release_input, handoff_input] = project_pointer_button(
            &mut capture,
            &targets,
            release,
            primary,
            ButtonState::Released,
        );
        let release_input = release_input.expect("release should use the captured target");
        let handoff_input = handoff_input.expect("final release should republish the pointer");

        assert_eq!(release_input.location.target, laptop_target);
        assert!(matches!(
            release_input.action,
            PointerAction::Release(PointerButton::Primary)
        ));
        assert_eq!(handoff_input.location.target, external_target);
        assert_eq!(handoff_input.location.position, Vec2::new(100.0, 1075.0));
        assert!(matches!(
            handoff_input.action,
            PointerAction::Move { delta } if delta == Vec2::ZERO
        ));
    }

    #[test]
    fn pointer_target_handoff_waits_for_the_final_held_button() {
        let (targets, external_target, laptop_target) = stacked_targets();
        let mut capture = CapturedPointerTarget::default();
        let primary = LinuxButtonCode(0x110);
        let secondary = LinuxButtonCode(0x111);
        let press = InputPosition::new(100.0, 1085.0);
        let release = InputPosition::new(100.0, 1075.0);
        project_pointer_button(&mut capture, &targets, press, primary, ButtonState::Pressed);
        project_pointer_button(
            &mut capture,
            &targets,
            press,
            secondary,
            ButtonState::Pressed,
        );

        let [primary_release, early_handoff] = project_pointer_button(
            &mut capture,
            &targets,
            release,
            primary,
            ButtonState::Released,
        );
        assert_eq!(
            primary_release
                .expect("primary release should retain capture")
                .location
                .target,
            laptop_target
        );
        assert!(early_handoff.is_none());

        let [secondary_release, final_handoff] = project_pointer_button(
            &mut capture,
            &targets,
            release,
            secondary,
            ButtonState::Released,
        );
        assert!(matches!(
            secondary_release
                .expect("secondary release should use the captured target")
                .action,
            PointerAction::Release(PointerButton::Secondary)
        ));
        assert_eq!(
            final_handoff
                .expect("final release should hand off the pointer")
                .location
                .target,
            external_target
        );
    }

    #[test]
    fn live_output_reflow_updates_projected_local_coordinates_in_place() {
        let external_target = NormalizedRenderTarget::TextureView(ManualTextureViewHandle(1));
        let laptop_target = NormalizedRenderTarget::TextureView(ManualTextureViewHandle(2));
        let external = OutputConfiguration::new(
            OutputId::new(1),
            Extent::new(1_920, 1_080),
            OutputScale::new(1.0).expect("test scale should be valid"),
            LogicalPoint::ZERO,
            false,
            None,
        )
        .expect("external output should be valid");
        let laptop = OutputConfiguration::new(
            OutputId::new(2),
            Extent::new(2_240, 1_400),
            OutputScale::new(1.25).expect("test scale should be valid"),
            LogicalPoint::new(64.0, 1_080.0),
            true,
            None,
        )
        .expect("laptop output should be valid");
        let mut targets = InputTargets(vec![
            InputOutputTarget {
                configuration: external,
                target: external_target,
            },
            InputOutputTarget {
                configuration: laptop,
                target: laptop_target.clone(),
            },
        ]);
        let mut capture = CapturedPointerTarget::default();
        capture.press(
            &targets,
            InputPosition::new(164.0, 1_180.0),
            LinuxButtonCode(0x110),
        );
        let reflowed_laptop = laptop
            .with_scale(OutputScale::new(1.5).expect("test scale should be valid"))
            .and_then(|output| output.with_position(LogicalPoint::new(213.333_34, 1_080.0)))
            .expect("reflowed laptop should remain valid");

        targets.update_configurations(&[external, reflowed_laptop]);
        let (target, local) = capture
            .project(&targets, InputPosition::new(313.333_34, 1_180.0))
            .expect("captured pointer should use the reflowed laptop geometry");

        assert_eq!(target, &laptop_target);
        assert!((local.x - 100.0).abs() < 0.001);
        assert_eq!(local.y, 100.0);
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
