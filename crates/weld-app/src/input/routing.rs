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
    ui::{ComputedNode, UiGlobalTransform, UiScale},
};
use leafwing_input_manager::plugin::InputManagerSystem;

use super::{
    InputSystems,
    raw::{ButtonState, LinuxButtonCode, RawSeatEvent, RawSeatEventKind},
    state::{InputUpdateTime, PendingSeatInput, PointerPositionState},
};
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
    picked_nodes: Query<(&SurfaceInputNode, &ComputedNode, &UiGlobalTransform)>,
    ui_scale: Res<UiScale>,
    update_time: Res<InputUpdateTime>,
    resources: InputRoutingResources,
) {
    let InputRoutingResources {
        mut pending,
        mut effects,
        mut routing,
    } = resources;

    replay_input_batch(&mut routing, pending.0.drain(..));

    // Client protocol delivery is not performed here. Core retains this
    // frame-published target and independently forwards every raw event using
    // it until a later frame publishes a different target.
    if !routing.pressed_buttons.is_empty() {
        return;
    }
    let target = picked_surface_target(&pointers, &picked_nodes, ui_scale.0);
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

fn picked_surface_target(
    pointers: &Query<(&PointerId, &PointerInteraction, &PointerLocation)>,
    picked_nodes: &Query<(&SurfaceInputNode, &ComputedNode, &UiGlobalTransform)>,
    ui_scale: f32,
) -> Option<SurfaceInputTarget> {
    let (_, interaction, location) = pointers
        .iter()
        .find(|(pointer, _, _)| **pointer == PointerId::Mouse)?;
    location.location()?;
    // The frontmost hit is authoritative. Falling through compositor-owned UI
    // to a client surface below it would leak raw input through decorations and
    // would also hand cursor ownership back to that client during shell grabs.
    frontmost_surface_target(interaction.iter().map(|(entity, _)| {
        let (surface, node, transform) = picked_nodes.get(*entity).ok()?;
        surface_input_target(*surface, node, transform, ui_scale)
    }))
}

fn frontmost_surface_target(
    candidates: impl IntoIterator<Item = Option<SurfaceInputTarget>>,
) -> Option<SurfaceInputTarget> {
    candidates.into_iter().next().flatten()
}

fn surface_input_target(
    surface: SurfaceInputNode,
    node: &ComputedNode,
    transform: &UiGlobalTransform,
    ui_scale: f32,
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
            ui_scale,
        ),
    })
}

fn compositor_to_surface_transform(
    inverse: bevy::math::Affine2,
    physical_size: bevy::math::Vec2,
    logical_per_physical: f32,
    local_origin: bevy::math::Vec2,
    ui_scale: f32,
) -> InputTransform {
    let matrix = inverse.matrix2 * (ui_scale * logical_per_physical);
    let logical_size = physical_size * logical_per_physical;
    let translation =
        inverse.translation * logical_per_physical + logical_size * 0.5 + local_origin;
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
    fn affine_mapping_inverts_translation_scale_and_rotation() {
        let physical_size = Vec2::new(240.0, 120.0);
        let logical_per_physical = 0.5;
        let local_origin = Vec2::new(7.0, 11.0);
        let ui_scale = 1.5;
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
            let compositor_position = global.transform_point2(local_centered) / ui_scale;
            let transform = compositor_to_surface_transform(
                global.inverse(),
                physical_size,
                logical_per_physical,
                local_origin,
                ui_scale,
            );
            let actual = transform.transform(super::super::raw::InputPosition::new(
                f64::from(compositor_position.x),
                f64::from(compositor_position.y),
            ));
            let expected = local_centered * logical_per_physical
                + physical_size * logical_per_physical * 0.5
                + local_origin;
            assert_position_close(actual, expected);
        }
    }

    #[test]
    fn frontmost_shell_hit_blocks_a_surface_below() {
        assert_eq!(
            frontmost_surface_target([None, Some(target(InputTransform::IDENTITY))]),
            None
        );
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
