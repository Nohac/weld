//! Frame-paced picking and protocol-neutral client focus publication.

use std::collections::{HashSet, VecDeque};

use bevy::{
    app::{App, PreUpdate},
    ecs::{
        resource::Resource,
        schedule::IntoScheduleConfigs,
        system::{Query, Res, ResMut, SystemParam},
        world::World,
    },
    picking::{
        PickingSystems,
        pointer::{PointerId, PointerInteraction, PointerLocation},
    },
    ui::{ComputedNode, ComputedUiTargetCamera, UiGlobalTransform},
};
use leafwing_input_manager::plugin::InputManagerSystem;

use super::{
    InputSystems,
    pointer_shortcuts::PublishedPointerTarget,
    raw::{ButtonState, LinuxButtonCode, RawSeatEvent, RawSeatEventKind},
    state::{InputUpdateTime, PendingSeatInput, PointerPositionState},
};
use crate::output::{OutputPosition, RendersOutput};
use crate::surface::SurfaceInputNode;
use weld_core::input::{InputTransform, SeatInputEffect, SeatInputEffectKind, SurfaceInputTarget};

pub(super) fn register(app: &mut App) {
    app.init_resource::<InputEffects>()
        .init_resource::<PointerRoutingState>()
        .add_systems(
            PreUpdate,
            resolve_input_effects
                .in_set(InputSystems::Resolve)
                .in_set(PickingSystems::PostHover)
                .after(InputManagerSystem::Update),
        );
}

#[derive(Resource, Default)]
struct InputEffects(VecDeque<SeatInputEffect>);

#[derive(Clone, Copy, Debug, PartialEq)]
struct ResolvedPointer {
    target: Option<SurfaceInputTarget>,
}

#[derive(Resource, Default)]
struct PointerRoutingState {
    pointer: PointerPositionState,
    last_sent: Option<ResolvedPointer>,
    pressed_buttons: HashSet<LinuxButtonCode>,
}

pub(crate) fn take_input_effects(world: &mut World) -> Vec<SeatInputEffect> {
    world
        .get_resource_mut::<InputEffects>()
        .map(|mut effects| effects.0.drain(..).collect())
        .unwrap_or_default()
}

fn resolve_input_effects(
    pointers: Query<(&PointerId, &PointerInteraction, &PointerLocation)>,
    picked_nodes: Query<(
        &SurfaceInputNode,
        &ComputedNode,
        &UiGlobalTransform,
        &ComputedUiTargetCamera,
    )>,
    cameras: Query<&RendersOutput>,
    output_positions: Query<&OutputPosition>,
    update_time: Res<InputUpdateTime>,
    resources: InputRoutingResources,
) {
    let InputRoutingResources {
        mut pending,
        mut effects,
        mut routing,
        mut published_target,
    } = resources;

    replay_input_batch(&mut routing, pending.0.drain(..));

    // Client protocol delivery is not performed here. Core retains this
    // frame-published target and independently forwards every raw event using
    // it until a later frame publishes a different target.
    if !routing.pressed_buttons.is_empty() {
        return;
    }
    let picked = picked_pointer_target(&pointers, &picked_nodes, &cameras, &output_positions);
    published_target.0 = routing
        .pointer
        .host_position
        .and_then(|_| picked.map(|picked| picked.entity));
    let target = picked.and_then(|picked| picked.surface);
    let current = routing
        .pointer
        .host_position
        .map(|_| ResolvedPointer { target });
    if let Some(effect) = publish_pointer_focus(
        routing.last_sent,
        current,
        routing.pointer.last_known_position,
        update_time.0,
    ) {
        effects.0.push_back(effect);
        routing.last_sent = current;
    }
}

#[derive(SystemParam)]
struct InputRoutingResources<'w> {
    pending: ResMut<'w, PendingSeatInput>,
    effects: ResMut<'w, InputEffects>,
    routing: ResMut<'w, PointerRoutingState>,
    published_target: ResMut<'w, PublishedPointerTarget>,
}

fn replay_input_batch(
    routing: &mut PointerRoutingState,
    events: impl IntoIterator<Item = RawSeatEvent>,
) {
    for raw_event in events {
        match raw_event.event {
            RawSeatEventKind::PointerMotion { position } => routing.pointer.apply(position),
            RawSeatEventKind::PointerLeft { position } => {
                routing.pointer.apply(position);
                routing.pointer.clear_host();
            }
            RawSeatEventKind::PointerButton {
                position,
                button,
                state,
            } => {
                if let Some(position) = position {
                    routing.pointer.apply(position);
                }
                match state {
                    ButtonState::Pressed => {
                        routing.pressed_buttons.insert(button);
                    }
                    ButtonState::Released => {
                        routing.pressed_buttons.remove(&button);
                    }
                }
            }
            RawSeatEventKind::PointerAxis { position, .. } => {
                if let Some(position) = position {
                    routing.pointer.apply(position);
                }
            }
            RawSeatEventKind::HostFocusLost => {
                routing.pointer.clear_host();
                routing.pressed_buttons.clear();
            }
            RawSeatEventKind::PointerGesture { .. } | RawSeatEventKind::Keyboard { .. } => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PickedPointerTarget {
    entity: bevy::ecs::entity::Entity,
    surface: Option<SurfaceInputTarget>,
}

fn picked_pointer_target(
    pointers: &Query<(&PointerId, &PointerInteraction, &PointerLocation)>,
    picked_nodes: &Query<(
        &SurfaceInputNode,
        &ComputedNode,
        &UiGlobalTransform,
        &ComputedUiTargetCamera,
    )>,
    cameras: &Query<&RendersOutput>,
    output_positions: &Query<&OutputPosition>,
) -> Option<PickedPointerTarget> {
    let (_, interaction, location) = pointers
        .iter()
        .find(|(pointer, _, _)| **pointer == PointerId::Mouse)?;
    location.location()?;
    // The frontmost hit is authoritative. Falling through compositor-owned UI
    // to a client surface below it would leak raw input through decorations and
    // would also hand cursor ownership back to that client during shell grabs.
    let (entity, _) = interaction.iter().next()?;
    let surface = (|| {
        let (surface, node, transform, target_camera) = picked_nodes.get(*entity).ok()?;
        let camera = target_camera.get()?;
        let output = cameras.get(camera).ok()?.0;
        let output_position = output_positions.get(output).ok()?.0;
        surface_input_target(*surface, node, transform, output_position)
    })();
    Some(PickedPointerTarget {
        entity: *entity,
        surface,
    })
}

fn surface_input_target(
    surface: SurfaceInputNode,
    node: &ComputedNode,
    transform: &UiGlobalTransform,
    output_position: bevy::math::Vec2,
) -> Option<SurfaceInputTarget> {
    let inverse = transform.try_inverse()?;
    Some(SurfaceInputTarget {
        surface: surface.surface,
        layer: surface.layer,
        transform: compositor_to_surface_transform(
            inverse,
            node.size(),
            node.inverse_scale_factor(),
            surface.local_origin,
            output_position,
        ),
    })
}

fn compositor_to_surface_transform(
    inverse: bevy::math::Affine2,
    physical_size: bevy::math::Vec2,
    logical_per_physical: f32,
    local_origin: bevy::math::Vec2,
    output_position: bevy::math::Vec2,
) -> InputTransform {
    let matrix = inverse.matrix2;
    let logical_size = physical_size * logical_per_physical;
    let translation =
        inverse.translation * logical_per_physical + logical_size * 0.5 + local_origin
            - matrix * output_position;
    InputTransform {
        xx: f64::from(matrix.x_axis.x),
        xy: f64::from(matrix.y_axis.x),
        yx: f64::from(matrix.x_axis.y),
        yy: f64::from(matrix.y_axis.y),
        x: f64::from(translation.x),
        y: f64::from(translation.y),
    }
}

fn publish_pointer_focus(
    previous: Option<ResolvedPointer>,
    current: Option<ResolvedPointer>,
    position: super::raw::InputPosition,
    time: u32,
) -> Option<SeatInputEffect> {
    (previous != current).then(|| {
        SeatInputEffect::new(
            SeatInputEffectKind::PointerFocus {
                position,
                target: current.and_then(|pointer| pointer.target),
            },
            time,
        )
    })
}

#[cfg(test)]
mod tests {
    use std::f32::consts::FRAC_PI_2;

    use bevy::math::{Affine2, Vec2};

    use super::*;
    use crate::surface::{SurfaceId, SurfaceLayerId};

    fn target(transform: InputTransform) -> SurfaceInputTarget {
        SurfaceInputTarget {
            surface: SurfaceId::new(1),
            layer: SurfaceLayerId::new(2),
            transform,
        }
    }

    fn assert_position_close(actual: super::super::raw::InputPosition, expected: Vec2) {
        assert!((actual.x - f64::from(expected.x)).abs() < 0.0001);
        assert!((actual.y - f64::from(expected.y)).abs() < 0.0001);
    }

    #[test]
    fn affine_mapping_converts_compositor_global_logical_to_surface_local() {
        let physical_size = Vec2::new(240.0, 120.0);
        let local_origin = Vec2::new(7.0, 11.0);
        for logical_per_physical in [1.0, 0.8] {
            for output_position in [Vec2::ZERO, Vec2::new(0.0, 1_080.0)] {
                for global in [
                    Affine2::IDENTITY,
                    Affine2::from_translation(Vec2::new(80.0, -25.0)),
                    Affine2::from_scale_angle_translation(
                        Vec2::new(1.25, 0.75),
                        FRAC_PI_2,
                        Vec2::new(320.0, 180.0),
                    ),
                ] {
                    let local_centered = Vec2::new(24.0, -18.0);
                    let target_physical = global.transform_point2(local_centered);
                    let compositor_global =
                        output_position + target_physical * logical_per_physical;
                    let transform = compositor_to_surface_transform(
                        global.inverse(),
                        physical_size,
                        logical_per_physical,
                        local_origin,
                        output_position,
                    );
                    let actual = transform.transform(super::super::raw::InputPosition::new(
                        f64::from(compositor_global.x),
                        f64::from(compositor_global.y),
                    ));
                    let expected = local_centered * logical_per_physical
                        + physical_size * logical_per_physical * 0.5
                        + local_origin;
                    assert_position_close(actual, expected);
                }
            }
        }
    }

    #[test]
    fn stationary_pointer_republishes_a_changed_surface_transform() {
        let previous = Some(ResolvedPointer {
            target: Some(target(InputTransform::IDENTITY)),
        });
        let mut moved = InputTransform::IDENTITY;
        moved.x = -40.0;
        let current = Some(ResolvedPointer {
            target: Some(target(moved)),
        });

        assert!(
            publish_pointer_focus(
                previous,
                current,
                super::super::raw::InputPosition::new(100.0, 80.0),
                12,
            )
            .is_some()
        );
    }
}
